//! 原生 `.xpkg` 的提交阶段：暂存解包、ABI 校验、生命周期脚本、升级替换和数据库提交。
//!
//! 所有 `.xpkg` 必须先下载/构建到本地；脚本在 hooks 模块的 chroot 沙箱中运行。

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::abi::{AbiGuard, MAX_GLIBC_VERSION};
use crate::capability::Version;
use crate::hooks::{
    self, HookScript, POST_INSTALL, POST_UPGRADE, PRE_INSTALL, PRE_REMOVE, PRE_UPGRADE,
};
use crate::solver::SelectedPackage;
use crate::state::{LocalDb, PackageInstall};
use crate::transaction::{self, Transaction};
use crate::triggers;
use crate::ui;
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

        let file_count = ui::count(files.len(), "file", "files", "个文件");
        ui::step(&ui::pick(
            format!(
                "解包并校验 {} {} -> 暂存区 ({file_count})",
                package.name, package.version
            ),
            format!(
                "Unpacking and checking {} {} -> staging ({file_count})",
                package.name, package.version
            ),
        ));
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
    let mut transaction = Transaction::begin(&rootfs, Some(db))?;
    for install in &installs {
        transaction.record_install(&install.package.name, &install.version_raw, &install.origin)?;
    }

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
            let rollback = transaction.rollback();
            return Err(join_rollback(error, rollback));
        }
    }

    let all_files = installs
        .iter()
        .flat_map(|install| install.files.iter().cloned())
        .collect::<Vec<_>>();
    if let Err(error) = xpkg::publish_files_txn(&mut transaction, &staging, &all_files) {
        let rollback = transaction.rollback();
        return Err(join_rollback(error, rollback));
    }
    ui::ok(&ui::pick(
        format!(
            "ABI 校验通过（GLIBC 基线 {MAX_GLIBC_VERSION}），发布到 {}",
            rootfs.path().display()
        ),
        format!(
            "ABI check passed (GLIBC baseline {MAX_GLIBC_VERSION}); published to {}",
            rootfs.path().display()
        ),
    ));

    let new_files = installs
        .iter()
        .flat_map(|install| install.files.iter().cloned())
        .collect::<BTreeSet<_>>();
    let (removed_files, directories) =
        match remove_obsolete_files(db, &mut transaction, &installs, &upgrades, &new_files) {
            Ok(result) => result,
            Err(error) => {
                let rollback = transaction.rollback();
                return Err(join_rollback(error, rollback));
            }
        };

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
            let rollback = transaction.rollback();
            return Err(join_rollback(error, rollback));
        }
    }

    if let Err(error) = db.commit(&installs) {
        let rollback = transaction.rollback();
        return Err(join_rollback(error, rollback));
    }
    transaction.commit();
    let _ = transaction::remove_empty_directories(rootfs.path(), &directories);
    let removed_for_triggers = removed_files
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>();
    triggers::after_install(&rootfs, &all_files, &removed_for_triggers, no_hooks);
    let package_count = ui::count(installs.len(), "package", "packages", "个包");
    ui::ok(&ui::pick(
        format!("提交完成: 已写入 {package_count}"),
        format!("Transaction committed: {package_count} installed"),
    ));
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
///
/// 文件先移入事务快照而不是直接删除；返回被删除的相对路径和需要在提交后
/// 清理的父目录。
fn remove_obsolete_files(
    db: &LocalDb,
    transaction: &mut Transaction,
    installs: &[PackageInstall],
    upgrades: &[Upgrade],
    new_files: &BTreeSet<String>,
) -> Result<(Vec<PathBuf>, BTreeSet<PathBuf>), String> {
    let mut removed = Vec::new();
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
            transaction.remove_file(&relative)?;
            removed.push(relative);
        }
    }
    let directories = transaction::parent_directories(removed.clone());
    Ok((removed, directories))
}

fn join_rollback(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => format!("{error}; 已回滚并恢复旧版本"),
        Err(rollback_error) => format!("{error}; 回滚/恢复失败: {rollback_error}"),
    }
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
