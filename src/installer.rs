//! 原生 `.xpkg` 的提交阶段：暂存解包、ABI 校验、生命周期脚本、升级替换和数据库提交。
//!
//! 所有 `.xpkg` 必须先下载/构建到本地；脚本在 hooks 模块的 chroot 沙箱中运行。

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::abi::{AbiGuard, MAX_GLIBC_VERSION};
use crate::capability::Version;
use crate::hooks::{
    self, HookScript, POST_INSTALL, POST_UPGRADE, PRE_INSTALL, PRE_REMOVE, PRE_UPGRADE,
};
use crate::solver::SelectedPackage;
use crate::state::{LocalDb, PackageInstall};
use crate::xpkg::{self, Rootfs};

/// 一个正在升级的包。
struct Upgrade {
    install_index: usize,
    package_name: String,
    old_origin: String,
    old_version_raw: String,
    old_files: Vec<String>,
    old_hooks: Vec<HookScript>,
}

/// 提交阶段：暂存解包、ABI 校验、执行安装/升级 hook、发布、清理旧文件、写数据库。
pub fn install_changes(
    db: &mut LocalDb,
    changes: &[SelectedPackage],
    archives: &HashMap<(String, Version), PathBuf>,
    rootfs_path: &Path,
    no_hooks: bool,
    allow_downgrade: bool,
) -> Result<(), String> {
    let staging_dir = StagingDir::create()?;
    let staging = Rootfs::prepare(staging_dir.path())?;
    let abi_guard = AbiGuard::new(MAX_GLIBC_VERSION)?;

    let mut installs = Vec::with_capacity(changes.len());
    for package in changes {
        let archive = archives
            .get(&(package.name.clone(), package.version.clone()))
            .ok_or_else(|| {
                format!(
                    "安装计划中的包 {} {} 没有对应的 .xpkg 路径",
                    package.name, package.version
                )
            })?;
        let files = xpkg::install_package(archive, package, &staging)?;
        let hooks = xpkg::read_hooks(archive)?;

        for file in &files {
            let staged_file = staging.path().join(file);
            let metadata = fs::symlink_metadata(&staged_file).map_err(|error| {
                format!("读取暂存文件 `{}` 失败: {error}", staged_file.display())
            })?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            abi_guard.check_file(&package.name, file, &staged_file)?;
        }

        println!(
            "解包并校验 {} {} -> 暂存区 ({} 个文件)",
            package.name,
            package.version,
            files.len()
        );
        installs.push(PackageInstall {
            package: package.clone(),
            version_raw: package.version.to_string(),
            origin: "native".to_string(),
            files,
            hooks,
        });
    }

    let installed = db.list_installed()?;
    let mut upgrades = Vec::new();
    for (index, install) in installs.iter().enumerate() {
        let Some(old) = installed
            .iter()
            .find(|package| package.name == install.package.name)
        else {
            continue;
        };
        if install.package.version < old.version && !allow_downgrade {
            return Err(format!(
                "拒绝降级: {} {} -> {}；如确需降级请加 --allow-downgrade",
                old.name, old.version_raw, install.package.version
            ));
        }
        if install.package.version == old.version {
            continue;
        }
        upgrades.push(Upgrade {
            install_index: index,
            package_name: install.package.name.clone(),
            old_origin: old.origin.clone(),
            old_version_raw: old.version_raw.clone(),
            old_files: db.list_files(&install.package.name)?,
            old_hooks: db.list_hooks(&install.package.name)?,
        });
    }

    let rootfs = Rootfs::prepare(rootfs_path)?;
    let backup = Backup::create(&rootfs, &upgrades)?;

    // 旧版本是 native 时执行新包的 pre_upgrade；外部包先执行旧包 pre_remove。
    if !no_hooks {
        let pre_result = (|| -> Result<(), String> {
            for upgrade in &upgrades {
                if upgrade.old_origin == "native" {
                    let install = &installs[upgrade.install_index];
                    run_preferred_phase(install, PRE_UPGRADE, PRE_INSTALL, &rootfs)?;
                } else {
                    run_old_phase(
                        &upgrade.package_name,
                        &upgrade.old_version_raw,
                        &upgrade.old_hooks,
                        PRE_REMOVE,
                        &rootfs,
                    )?;
                    run_preferred_phase(
                        &installs[upgrade.install_index],
                        PRE_INSTALL,
                        PRE_INSTALL,
                        &rootfs,
                    )?;
                }
            }
            for (index, install) in installs.iter().enumerate() {
                if upgrades
                    .iter()
                    .any(|upgrade| upgrade.install_index == index)
                {
                    continue;
                }
                run_preferred_phase(install, PRE_INSTALL, PRE_INSTALL, &rootfs)?;
            }
            Ok(())
        })();
        if let Err(error) = pre_result {
            let rollback = backup.restore(&rootfs);
            return Err(join_rollback(error, rollback));
        }
    }

    let all_files = installs
        .iter()
        .flat_map(|install| install.files.iter().cloned())
        .collect::<Vec<_>>();
    if let Err(error) = xpkg::publish_files(&staging, &rootfs, &all_files) {
        let rollback = rollback_install(db, &installs, &rootfs, &backup);
        return Err(join_rollback(error, rollback));
    }
    println!(
        "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
        rootfs.path().display()
    );

    let new_files = installs
        .iter()
        .flat_map(|install| install.files.iter().cloned())
        .collect::<BTreeSet<_>>();
    if let Err(error) = remove_obsolete_files(db, &rootfs, &installs, &upgrades, &new_files) {
        let rollback = rollback_install(db, &installs, &rootfs, &backup);
        return Err(join_rollback(error, rollback));
    }

    if !no_hooks {
        let post_result = (|| -> Result<(), String> {
            for upgrade in &upgrades {
                let install = &installs[upgrade.install_index];
                if upgrade.old_origin == "native" {
                    run_preferred_phase(install, POST_UPGRADE, POST_INSTALL, &rootfs)?;
                } else {
                    run_phase(install, POST_INSTALL, &rootfs)?;
                }
            }
            for (index, install) in installs.iter().enumerate() {
                if upgrades
                    .iter()
                    .any(|upgrade| upgrade.install_index == index)
                {
                    continue;
                }
                run_phase(install, POST_INSTALL, &rootfs)?;
            }
            Ok(())
        })();
        if let Err(error) = post_result {
            let rollback = rollback_install(db, &installs, &rootfs, &backup);
            return Err(join_rollback(error, rollback));
        }
    }

    if let Err(error) = db.commit(&installs) {
        let rollback = rollback_install(db, &installs, &rootfs, &backup);
        return Err(join_rollback(error, rollback));
    }
    println!("提交完成: 已写入 {} 个包", installs.len());
    Ok(())
}

/// 执行指定阶段的 hook。
fn run_phase(install: &PackageInstall, phase: &str, rootfs: &Rootfs) -> Result<(), String> {
    run_old_phase(
        &install.package.name,
        &install.version_raw,
        &install.hooks,
        phase,
        rootfs,
    )
}

/// 优先执行 preferred 阶段；没有该阶段脚本时执行 fallback。
fn run_preferred_phase(
    install: &PackageInstall,
    preferred: &str,
    fallback: &str,
    rootfs: &Rootfs,
) -> Result<(), String> {
    if install.hooks.iter().any(|hook| hook.phase == preferred) {
        run_phase(install, preferred, rootfs)
    } else {
        run_phase(install, fallback, rootfs)
    }
}

fn run_old_phase(
    package_name: &str,
    version: &str,
    hooks: &[HookScript],
    phase: &str,
    rootfs: &Rootfs,
) -> Result<(), String> {
    for hook in hooks {
        if hook.phase == phase {
            hooks::run_hook(phase, package_name, version, rootfs, &hook.script)?;
        }
    }
    Ok(())
}

/// 清理旧版本有、新版本没有，且未被其他包/本次其他新包引用的文件。
fn remove_obsolete_files(
    db: &LocalDb,
    rootfs: &Rootfs,
    installs: &[PackageInstall],
    upgrades: &[Upgrade],
    new_files: &BTreeSet<String>,
) -> Result<(), String> {
    let mut directories = BTreeSet::new();
    for upgrade in upgrades {
        for file in &upgrade.old_files {
            if new_files.contains(file) {
                continue;
            }
            let package_name = &installs[upgrade.install_index].package.name;
            if db.file_claimed_by_other(package_name, file)? {
                continue;
            }
            let relative = xpkg::sanitize_relative(Path::new(file))
                .map_err(|error| format!("旧文件路径 `{file}` 非法: {error}"))?;
            let target = rootfs.path().join(&relative);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    return Err(format!("旧文件 `{}` 是目录", target.display()))
                }
                Ok(_) => fs::remove_file(&target).map_err(|error| {
                    format!("删除废弃文件 `{}` 失败: {error}", target.display())
                })?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("检查废弃文件 `{}` 失败: {error}", target.display()))
                }
            }
            collect_parent_directories(&relative, &mut directories);
        }
    }
    remove_empty_directories(rootfs.path(), &directories)
}

/// 回滚已发布文件，再恢复旧版本备份。
fn rollback_install(
    db: &LocalDb,
    installs: &[PackageInstall],
    rootfs: &Rootfs,
    backup: &Backup,
) -> Result<(), String> {
    rollback_published(db, installs, rootfs)?;
    backup.restore(rootfs)
}

fn rollback_published(
    db: &LocalDb,
    installs: &[PackageInstall],
    rootfs: &Rootfs,
) -> Result<(), String> {
    let mut directories = BTreeSet::new();
    for install in installs {
        for file in &install.files {
            if db.file_claimed_by_other(&install.package.name, file)? {
                continue;
            }
            let relative = xpkg::sanitize_relative(Path::new(file))
                .map_err(|error| format!("回滚路径 `{file}` 非法: {error}"))?;
            let target = rootfs.path().join(&relative);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    return Err(format!("回滚目标 `{}` 是目录", target.display()))
                }
                Ok(_) => fs::remove_file(&target)
                    .map_err(|error| format!("回滚删除 `{}` 失败: {error}", target.display()))?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("回滚检查 `{}` 失败: {error}", target.display())),
            }
            collect_parent_directories(&relative, &mut directories);
        }
    }
    remove_empty_directories(rootfs.path(), &directories)
}

fn join_rollback(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => format!("{error}; 已回滚并恢复旧版本"),
        Err(rollback_error) => format!("{error}; 回滚/恢复失败: {rollback_error}"),
    }
}

/// 旧版本文件备份，用于升级失败时恢复。
struct Backup {
    root: PathBuf,
    entries: Vec<(PathBuf, PathBuf)>,
}

impl Backup {
    fn create(rootfs: &Rootfs, upgrades: &[Upgrade]) -> Result<Self, String> {
        if upgrades.is_empty() {
            return Ok(Self {
                root: PathBuf::new(),
                entries: Vec::new(),
            });
        }
        let base = PathBuf::from("/var/tmp/xrpkg-upgrade");
        fs::create_dir_all(&base).map_err(|error| format!("创建升级备份目录失败: {error}"))?;
        let root = base.join(format!("backup-{}-{}", std::process::id(), nonce()));
        fs::create_dir(&root)
            .map_err(|error| format!("创建升级备份 `{}` 失败: {error}", root.display()))?;

        let mut entries = Vec::new();
        for upgrade in upgrades {
            for file in &upgrade.old_files {
                let relative = xpkg::sanitize_relative(Path::new(file))
                    .map_err(|error| format!("备份路径 `{file}` 非法: {error}"))?;
                let source = rootfs.path().join(&relative);
                let metadata = match fs::symlink_metadata(&source) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!("备份检查 `{}` 失败: {error}", source.display()))
                    }
                };
                let destination = root.join(&relative);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|error| format!("创建备份目录失败: {error}"))?;
                }
                if metadata.file_type().is_symlink() {
                    let target = fs::read_link(&source).map_err(|error| {
                        format!("读取备份符号链接 `{}` 失败: {error}", source.display())
                    })?;
                    create_symlink(&target, &destination)?;
                } else if metadata.is_file() {
                    fs::copy(&source, &destination).map_err(|error| {
                        format!("备份文件 `{}` 失败: {error}", source.display())
                    })?;
                } else {
                    return Err(format!("备份目标 `{}` 不是文件/符号链接", source.display()));
                }
                entries.push((relative, destination));
            }
        }
        Ok(Self { root, entries })
    }

    fn restore(&self, rootfs: &Rootfs) -> Result<(), String> {
        for (relative, backup) in &self.entries {
            let target = rootfs.path().join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| format!("恢复创建目录失败: {error}"))?;
            }
            match fs::symlink_metadata(&target) {
                Ok(_) => {
                    let _ = fs::remove_file(&target);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("恢复检查 `{}` 失败: {error}", target.display())),
            }
            let metadata = fs::symlink_metadata(backup)
                .map_err(|error| format!("读取备份 `{}` 失败: {error}", backup.display()))?;
            if metadata.file_type().is_symlink() {
                let link =
                    fs::read_link(backup).map_err(|error| format!("读取备份链接失败: {error}"))?;
                create_symlink(&link, &target)?;
            } else {
                fs::copy(backup, &target)
                    .map_err(|error| format!("恢复文件 `{}` 失败: {error}", target.display()))?;
            }
        }
        Ok(())
    }
}

impl Drop for Backup {
    fn drop(&mut self) {
        if !self.root.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn create_symlink(target: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, destination)
            .map_err(|error| format!("创建符号链接 `{}` 失败: {error}", destination.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (target, destination);
        Err("当前平台不支持符号链接".to_string())
    }
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn collect_parent_directories(relative: &Path, directories: &mut BTreeSet<PathBuf>) {
    let mut current = relative.parent();
    while let Some(directory) = current {
        if directory.as_os_str().is_empty() {
            break;
        }
        directories.insert(directory.to_path_buf());
        current = directory.parent();
    }
}

fn remove_empty_directories(root: &Path, directories: &BTreeSet<PathBuf>) -> Result<(), String> {
    let mut ordered = directories.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    for relative in ordered {
        let directory = root.join(relative);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "检查空目录 `{}` 失败: {error}",
                    directory.display()
                ))
            }
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| format!("读取目录 `{}` 失败: {error}", directory.display()))?;
        if entries.next().is_some() {
            continue;
        }
        fs::remove_dir(&directory)
            .map_err(|error| format!("删除空目录 `{}` 失败: {error}", directory.display()))?;
    }
    Ok(())
}

/// 临时暂存目录；离开作用域时自动清理。
struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
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

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
