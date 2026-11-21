//! 基于 `installed_files` 的精确卸载。
//!
//! 只删除数据库记录中属于该包的路径；共享文件保留，空目录自底向上清理。

use std::collections::BTreeSet;
use std::path::Path;

use crate::hooks::{self, POST_REMOVE, PRE_REMOVE};
use crate::state::LocalDb;
use crate::transaction::{self, Transaction};
use crate::xpkg::{sanitize_relative, Rootfs};

/// 一次卸载的结果统计。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoveReport {
    pub removed_files: usize,
    pub skipped_shared: usize,
    pub missing: usize,
    pub removed_dirs: usize,
}

/// 卸载包：先删文件系统路径，全部成功后再在事务中清理数据库。
pub fn remove_package(
    db: &mut LocalDb,
    rootfs: &Rootfs,
    package_name: &str,
    no_hooks: bool,
) -> Result<RemoveReport, String> {
    if package_name.is_empty() {
        return Err("包名不能为空".to_string());
    }
    if !db.package_exists(package_name)? {
        return Err(format!("包 `{package_name}` 未安装"));
    }

    let files = db.list_files(package_name)?;
    let hooks = db.list_hooks(package_name)?;
    let version = db
        .list_installed()?
        .into_iter()
        .find(|package| package.name == package_name)
        .map(|package| package.version_raw)
        .unwrap_or_default();

    // 防呆：如果该包独占的文件一个都不存在，极可能是 --root 指向了错误的
    // Rootfs。此时绝不能只删数据库记录，让真实系统文件变成“孤儿”。
    let mut owned = 0usize;
    let mut present = 0usize;
    for file in &files {
        if db.file_claimed_by_other(package_name, file)? {
            continue;
        }
        owned += 1;
        let relative = sanitize_relative(Path::new(file))
            .map_err(|error| format!("数据库中的文件路径 `{file}` 非法: {error}"))?;
        if std::fs::symlink_metadata(rootfs.path().join(&relative)).is_ok() {
            present += 1;
        }
    }
    if owned > 0 && present == 0 {
        return Err(format!(
            "目标 Rootfs `{}` 中找不到包 `{package_name}` 的任何独占文件；拒绝只删除数据库记录，请检查 --root",
            rootfs.path().display()
        ));
    }

    if !no_hooks {
        for hook in &hooks {
            if hook.phase == PRE_REMOVE {
                hooks::run_hook(PRE_REMOVE, package_name, &version, rootfs, &hook.script)?;
            }
        }
    }
    let mut report = RemoveReport::default();
    let mut directories = BTreeSet::new();
    let mut transaction = Transaction::begin(rootfs, Some(db))?;
    transaction.record_remove(package_name)?;

    for file in &files {
        let relative = sanitize_relative(Path::new(file))
            .map_err(|error| format!("数据库中的文件路径 `{file}` 非法: {error}"))?;

        if db.file_claimed_by_other(package_name, file)? {
            report.skipped_shared += 1;
            continue;
        }

        match transaction.remove_file(&relative) {
            Ok(true) => report.removed_files += 1,
            Ok(false) => report.missing += 1,
            Err(error) => {
                let rollback = transaction.rollback();
                return Err(join_rollback(error, rollback));
            }
        }
        directories.extend(transaction::parent_directories([relative]));
    }

    if !no_hooks {
        let post_result = (|| -> Result<(), String> {
            for hook in &hooks {
                if hook.phase == POST_REMOVE {
                    hooks::run_hook(POST_REMOVE, package_name, &version, rootfs, &hook.script)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = post_result {
            let rollback = transaction.rollback();
            return Err(join_rollback(error, rollback));
        }
    }

    // 文件和 post_remove 全部成功后才删除数据库记录；数据库失败则恢复快照。
    if let Err(error) = db.remove_package_records(package_name) {
        let rollback = transaction.rollback();
        return Err(join_rollback(error, rollback));
    }
    transaction.commit();

    // 数据库已经提交；空目录清理失败不影响包的卸载结果。
    report.removed_dirs =
        transaction::remove_empty_directories(rootfs.path(), &directories).unwrap_or(0);
    Ok(report)
}

/// 把卸载阶段的错误与回滚结果合并成一条可读信息。
fn join_rollback(error: String, rollback: Result<(), String>) -> String {
    match rollback {
        Ok(()) => format!("{error}; 已回滚文件删除"),
        Err(rollback_error) => format!("{error}; 回滚失败: {rollback_error}"),
    }
}
