//! 演示入口：Rootfs 初始化、内核/基础用户态构建、签名仓库安装/升级/降级/缓存清理、隔离构建、卸载/hooks，以及兼容轨道 .deb/.rpm/.pkg.tar.zst 的安装-记录-卸载闭环。

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
use xrpkg::hooks::{self, HookScript, POST_INSTALL, PRE_INSTALL};
use xrpkg::installer;
use xrpkg::metadata::load_repository;
use xrpkg::mirrors;
use xrpkg::repo;
use xrpkg::resolvo_solver::{self, incremental_plan};
use xrpkg::sign;
use xrpkg::solver::{Package, SelectedPackage};
use xrpkg::state::{InstalledPackage, LocalDb, PackageInstall};
use xrpkg::sync;
use xrpkg::uninstall;
use xrpkg::world;
use xrpkg::xpkg::{self, Rootfs};

/// 硬编码的本次安装目标，模拟 `install app`。
const INSTALL_TARGET: &str = "app";

/// 默认安装根目录；可以通过 `--root <dir>` 覆盖。
const DEFAULT_ROOTFS: &str = "/var/tmp/xrpkg-rootfs";

fn main() {
    if let Err(error) = run() {
        eprintln!("错误: {error}");
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
        return Err(format!("安装请求 `{input}` 缺少包名"));
    }
    if name
        .chars()
        .any(|character| character.is_whitespace() || matches!(character, '/' | '\\' | '='))
    {
        return Err(format!("包名 `{name}` 非法"));
    }
    if version_text.is_empty() {
        return Err(format!("安装请求 `{input}` 缺少版本"));
    }
    let version = Version::parse(version_text)
        .map_err(|error| format!("安装请求 `{input}` 的版本非法: {error}"))?;
    Ok((name.to_string(), Some(version)))
}

/// 返回 `--db` 指定的数据库，否则使用项目内的 var/lib/xrpkg/local.db。
fn database_path(project_root: &Path, cli: &Cli) -> PathBuf {
    cli.db
        .clone()
        .unwrap_or_else(|| project_root.join("var/lib/xrpkg/local.db"))
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
        + usize::from(cli.build_world);
    if action_count > 1 {
        return Err("构建、卸载、仓库、同步和兼容轨道操作只能选择一个".to_string());
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
        println!();
        let db = LocalDb::open(database_path(project_root, &cli))?;
        list(&db)?;
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
                println!("==> 同步镜像 {}: {}", mirror.name, mirror.url);
                sync::sync_mirror_with_options(&mirror.url, &repo_dir, &deps_map, &options)?;
            }
        } else if let Some(target) = &cli.sync {
            let url = if mirrors::is_direct_location(target) {
                target.clone()
            } else {
                mirrors::resolve_target(&mirrors::load(&mirrors_path)?, target)?
            };
            println!("==> 同步镜像: {url}");
            sync::sync_mirror_with_options(&url, &repo_dir, &deps_map, &options)?;
        }
        return Ok(());
    }

    if let Some((key_id, file)) = &cli.repo_sign {
        let signature = sign::sign_detached(key_id, file)?;
        println!("已生成签名: {}", signature.display());
        return Ok(());
    }

    if let Some((repo_dir, package_path)) = &cli.repo_add {
        let index = match &cli.repo_add_key {
            Some(key_id) => repo::repo_add_signed(repo_dir, package_path, key_id)?,
            None => repo::repo_add(repo_dir, package_path)?,
        };
        println!(
            "仓库索引已更新: {} ({} 个包)",
            repo_dir.join("xrpkg-repo.toml").display(),
            index.packages.len()
        );
        return Ok(());
    }

    if cli.init {
        init_rootfs(&cli.rootfs)?;
        println!("基础 Rootfs 配置已写入: {}", cli.rootfs.display());
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
        println!(
            "缓存清理完成: 删除 {} 个包、{} 个签名，释放 {}",
            report.packages,
            report.signatures,
            format_bytes(report.bytes)
        );
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
            println!("没有找到包含 `{keyword}` 的包");
        } else {
            for hit in hits {
                println!("{}/{} {}", hit.repo, hit.name, hit.version);
            }
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
            if package_name.ends_with(".deb") {
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
                return Err(format!(
                    "无法识别外部包 `{package_name}`；支持 .deb、.rpm、.pkg.tar.zst"
                ));
            }
            println!();
            list(&db)?;
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
            println!(
                "Arch 兼容包: {} {} ({})",
                download.name,
                download.version,
                download.path.display()
            );
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
        println!();
        list(&db)?;
        return Ok(());
    }

    if let Some(package_name) = &cli.remove {
        let mut db = LocalDb::open(database_path(project_root, &cli))?;
        let rootfs = Rootfs::prepare(&cli.rootfs)?;
        let report = uninstall::remove_package(&mut db, &rootfs, package_name, cli.no_hooks)?;
        println!("卸载完成: {package_name}");
        println!("删除文件: {}", report.removed_files);
        println!("共享文件保留: {}", report.skipped_shared);
        println!("已不存在: {}", report.missing);
        println!("删除空目录: {}", report.removed_dirs);
        println!();
        list(&db)?;
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

    install(
        &mut db,
        &packages,
        INSTALL_TARGET,
        &project_root.join("repo"),
        &cli.rootfs,
        cli.no_hooks,
        false,
    )?;
    println!();
    list(&db)?;
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
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();

    if !arguments.is_empty() {
        let first = arguments.remove(0);
        match first.as_str() {
            "repo-add" => {
                if arguments.len() < 2 {
                    return Err("用法: xrpkg repo-add <repo_dir> <pkg.xpkg>".to_string());
                }
                let repo_dir = PathBuf::from(arguments.remove(0));
                let package = PathBuf::from(arguments.remove(0));
                repo_add = Some((repo_dir, package));
            }
            "repo-sign" => {
                if arguments.len() < 2 {
                    return Err("用法: xrpkg repo-sign <key_id> <file>".to_string());
                }
                let key_id = arguments.remove(0);
                let file = PathBuf::from(arguments.remove(0));
                repo_sign = Some((key_id, file));
            }
            "update" => update = true,
            "clean" => clean = true,
            "init" => init = true,
            "build-world" => build_world = true,
            "search" => {
                if arguments.is_empty() {
                    return Err("用法: xrpkg search <keyword>".to_string());
                }
                search = Some(arguments.remove(0));
            }
            "install" => {
                if arguments.is_empty() {
                    return Err("用法: xrpkg install <pkg_name>".to_string());
                }
                install = Some(arguments.remove(0));
            }
            "sync" => {
                if arguments.is_empty() {
                    return Err("用法: xrpkg sync <url|mirror_name> | xrpkg sync --all".to_string());
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
                    .ok_or_else(|| "`--root` 需要一个目录参数".to_string())?;
                rootfs = PathBuf::from(value);
            }
            "--config" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--config` 需要一个 repos.toml 路径".to_string())?;
                config = Some(PathBuf::from(value));
            }
            "--mirrors" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--mirrors` 需要一个 mirrors.toml 路径".to_string())?;
                mirrors = Some(PathBuf::from(value));
            }
            "--repo-dir" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--repo-dir` 需要一个仓库目录".to_string())?;
                repo_dir = Some(PathBuf::from(value));
            }
            "--no-hooks" => no_hooks = true,
            "--allow-downgrade" => allow_downgrade = true,
            "--prefer-arch" => prefer_arch = true,
            "--all" => sync_all = true,
            "--no-sandbox" => no_sandbox = true,
            "--no-deps" => no_deps = true,
            "--jobs" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--jobs` 需要一个正整数".to_string())?;
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| format!("`--jobs` 的值 `{value}` 不是正整数"))?;
                if parsed == 0 {
                    return Err("`--jobs` 必须大于 0".to_string());
                }
                jobs = Some(parsed);
            }
            "--key" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--key` 需要一个 GPG key id".to_string())?;
                repo_add_key = Some(value);
            }
            "--db" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--db` 需要一个 SQLite 路径".to_string())?;
                db = Some(PathBuf::from(value));
            }
            "--cache" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--cache` 需要一个目录".to_string())?;
                cache = Some(PathBuf::from(value));
            }
            "--deb" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--deb` 需要一个 .deb 文件路径".to_string())?;
                deb = Some(PathBuf::from(value));
            }
            "--rpm" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--rpm` 需要一个 .rpm 文件路径".to_string())?;
                rpm = Some(PathBuf::from(value));
            }
            "--arch" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--arch` 需要一个 .pkg.tar.zst 文件路径".to_string())?;
                arch = Some(PathBuf::from(value));
            }
            "--build" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--build` 需要一个构建脚本路径".to_string())?;
                build = Some(PathBuf::from(value));
            }
            "--remove" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "`--remove` 需要一个包名".to_string())?;
                remove = Some(value);
            }
            _ => {
                return Err(format!(
                    "未知参数 `{argument}`；支持 init/install/update/clean/search/sync/repo-add/repo-sign/build-world 子命令，或 `--root <dir>`、`--build <recipe>`、`--remove <pkg>`、`--deb <file>`、`--rpm <file>`、`--arch <file>`、`--config <file>`、`--mirrors <file>`、`--repo-dir <dir>`、`--cache <dir>`、`--key <gpg-key>`、`--allow-downgrade`、`--prefer-arch`、`--jobs <n>`、`--no-sandbox`、`--no-deps` 和 `--no-hooks`"
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
    println!("生成 .xpkg: {}", archive.display());

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
    println!();
    list(&db)?;
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

enum ExternalUpgrade {
    Fresh,
    AlreadyInstalled,
    Upgrade(Vec<String>),
}

/// 外部包升级前处理：旧包 prerm，并返回旧文件清单。
fn prepare_external_upgrade(
    db: &LocalDb,
    package_name: &str,
    new_version_raw: &str,
    rootfs: &Rootfs,
    no_hooks: bool,
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
    let old_files = db.list_files(package_name)?;
    if !no_hooks {
        let hooks = db.list_hooks(package_name)?;
        run_external_hooks(&hooks, "pre_remove", package_name, &old.version_raw, rootfs)?;
    }
    Ok(ExternalUpgrade::Upgrade(old_files))
}

/// 删除外部包旧版本有、新版本没有且未被共享的文件。
fn remove_external_obsolete(
    db: &LocalDb,
    package_name: &str,
    old_files: &[String],
    new_files: &[PathBuf],
    rootfs: &Rootfs,
) -> Result<(), String> {
    let new_files = new_files
        .iter()
        .map(|file| file.to_string_lossy().replace('\\', "/"))
        .collect::<std::collections::BTreeSet<_>>();
    let mut directories = std::collections::BTreeSet::new();
    for file in old_files {
        if new_files.contains(file) || db.file_claimed_by_other(package_name, file)? {
            continue;
        }
        let target = rootfs.path().join(file);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
                fs::remove_file(&target).map_err(|error| {
                    format!("删除废弃文件 `{}` 失败: {error}", target.display())
                })?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("检查废弃文件 `{}` 失败: {error}", target.display())),
        }
        let mut current = Path::new(file).parent();
        while let Some(directory) = current {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
    }
    let mut ordered = directories.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    for relative in ordered {
        let directory = rootfs.path().join(relative);
        if let Ok(mut entries) = fs::read_dir(&directory) {
            if entries.next().is_none() {
                let _ = fs::remove_dir(&directory);
            }
        }
    }
    Ok(())
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

/// post_install 失败时清理外部包已发布的文件。
fn rollback_external_files(files: &[PathBuf], rootfs: &Rootfs) -> Result<(), String> {
    let mut directories = std::collections::BTreeSet::new();
    for file in files {
        let target = rootfs.path().join(file);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
                fs::remove_file(&target)
                    .map_err(|error| format!("回滚删除 `{}` 失败: {error}", target.display()))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("回滚检查 `{}` 失败: {error}", target.display())),
        }
        let mut current = file.parent();
        while let Some(directory) = current {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
    }
    let mut ordered = directories.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    for relative in ordered {
        let directory = rootfs.path().join(&relative);
        if let Ok(mut entries) = fs::read_dir(&directory) {
            if entries.next().is_none() {
                let _ = fs::remove_dir(&directory);
            }
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
    println!("依赖预检:");
    if requirements.is_empty() {
        println!("  (无)");
    } else {
        for requirement in requirements {
            println!("  {requirement}");
        }
    }

    resolvo_solver::resolve_requirements(packages, requirements, installed)?;
    println!("依赖预检通过");
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
    println!("Arch 包: {} {}", metadata.pkgname, metadata.pkgver);
    if metadata.depends.is_empty() {
        println!("依赖: (无)");
    } else {
        println!("依赖:");
        for dependency in &metadata.depends {
            println!("  {dependency}");
        }
    }

    if no_deps {
        println!("依赖预检: 跳过（--no-deps）");
    } else {
        let requirements = metadata.mapped_requirements(mapper)?;
        preflight_requirements(packages, installed, &requirements)?;
    }

    let hooks = compat_arch::read_arch_hooks(arch_path)?;
    let target = Rootfs::prepare(rootfs_path)?;
    let old_files =
        match prepare_external_upgrade(db, &metadata.pkgname, &metadata.pkgver, &target, no_hooks)?
        {
            ExternalUpgrade::Fresh => None,
            ExternalUpgrade::AlreadyInstalled => {
                println!("{} 已经安装，无需操作", metadata.pkgname);
                return Ok(());
            }
            ExternalUpgrade::Upgrade(files) => Some(files),
        };
    if !no_hooks {
        run_external_hooks(
            &hooks,
            PRE_INSTALL,
            &metadata.pkgname,
            &metadata.pkgver,
            &target,
        )?;
    }

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

    let publish_paths = files
        .iter()
        .map(|file| file.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    xpkg::publish_files(&staging, &target, &publish_paths)?;

    if let Some(old_files) = &old_files {
        remove_external_obsolete(db, &metadata.pkgname, old_files, &files, &target)?;
    }

    if !no_hooks {
        if let Err(error) = run_external_hooks(
            &hooks,
            POST_INSTALL,
            &metadata.pkgname,
            &metadata.pkgver,
            &target,
        ) {
            let rollback = rollback_external_files(&files, &target);
            return Err(match rollback {
                Ok(()) => format!("{error}; 已回滚已发布的文件"),
                Err(rollback_error) => format!("{error}; 回滚失败: {rollback_error}"),
            });
        }
    }

    commit_external_package(
        db,
        &metadata.pkgname,
        &metadata.pkgver,
        "arch",
        &files,
        &hooks,
    )?;

    println!(
        "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
        target.path().display()
    );
    println!("安装文件 ({}):", files.len());
    for file in &files {
        println!("{}", file.display());
    }
    println!("兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks");
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
    println!(
        "RPM 包: {} {}{}-{}",
        metadata.name, epoch, metadata.version, metadata.release
    );
    if metadata.requires.is_empty() {
        println!("Requires: (无)");
    } else {
        println!("Requires:");
        for requirement in &metadata.requires {
            println!("  {requirement}");
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
    let old_files =
        match prepare_external_upgrade(db, &metadata.name, &hook_version, &target, no_hooks)? {
            ExternalUpgrade::Fresh => None,
            ExternalUpgrade::AlreadyInstalled => {
                println!("{} 已经安装，无需操作", metadata.name);
                return Ok(());
            }
            ExternalUpgrade::Upgrade(files) => Some(files),
        };
    if !no_hooks {
        run_external_hooks(&hooks, PRE_INSTALL, &metadata.name, &hook_version, &target)?;
    }

    let publish_paths = files
        .iter()
        .map(|file| file.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    xpkg::publish_files(&staging, &target, &publish_paths)?;

    if let Some(old_files) = &old_files {
        remove_external_obsolete(db, &metadata.name, old_files, &files, &target)?;
    }

    if !no_hooks {
        if let Err(error) =
            run_external_hooks(&hooks, POST_INSTALL, &metadata.name, &hook_version, &target)
        {
            let rollback = rollback_external_files(&files, &target);
            return Err(match rollback {
                Ok(()) => format!("{error}; 已回滚已发布的文件"),
                Err(rollback_error) => format!("{error}; 回滚失败: {rollback_error}"),
            });
        }
    }

    commit_external_package(db, &metadata.name, &hook_version, "rpm", &files, &hooks)?;

    println!(
        "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
        target.path().display()
    );
    println!("安装文件 ({}):", files.len());
    for file in &files {
        println!("{}", file.display());
    }
    println!("兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks");
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
    println!("Debian 包: {} {}", metadata.package, metadata.version);
    println!("Depends: {}", metadata.depends.as_deref().unwrap_or("(无)"));

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
    let old_files = match prepare_external_upgrade(
        db,
        &metadata.package,
        &metadata.version,
        &target,
        no_hooks,
    )? {
        ExternalUpgrade::Fresh => None,
        ExternalUpgrade::AlreadyInstalled => {
            println!("{} 已经安装，无需操作", metadata.package);
            return Ok(());
        }
        ExternalUpgrade::Upgrade(files) => Some(files),
    };
    if !no_hooks {
        run_external_hooks(
            &hooks,
            PRE_INSTALL,
            &metadata.package,
            &metadata.version,
            &target,
        )?;
    }

    let publish_paths = files
        .iter()
        .map(|file| file.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    xpkg::publish_files(&staging, &target, &publish_paths)?;

    if let Some(old_files) = &old_files {
        remove_external_obsolete(db, &metadata.package, old_files, &files, &target)?;
    }

    if !no_hooks {
        if let Err(error) = run_external_hooks(
            &hooks,
            POST_INSTALL,
            &metadata.package,
            &metadata.version,
            &target,
        ) {
            let rollback = rollback_external_files(&files, &target);
            return Err(match rollback {
                Ok(()) => format!("{error}; 已回滚已发布的文件"),
                Err(rollback_error) => format!("{error}; 回滚失败: {rollback_error}"),
            });
        }
    }

    commit_external_package(
        db,
        &metadata.package,
        &metadata.version,
        "deb",
        &files,
        &hooks,
    )?;

    println!(
        "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
        target.path().display()
    );
    println!("安装文件 ({}):", files.len());
    for file in &files {
        println!("{}", file.display());
    }
    println!("兼容轨道：已写入 installed_packages/installed_files 和 installed_hooks");
    Ok(())
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
        println!("{requested} 已经安装，无需操作");
        return Ok(());
    }

    println!("安装计划:");
    for package in &changes {
        println!("{} {}", package.name, package.version);
    }

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
    println!("已安装包:");
    if installed.is_empty() {
        println!("(空)");
    } else {
        for package in &installed {
            println!(
                "{} {} [{}]",
                package.name, package.version_raw, package.origin
            );
        }
    }
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
