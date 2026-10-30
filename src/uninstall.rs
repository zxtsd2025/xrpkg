//! 基于 `installed_files` 的精确卸载。
//!
//! 只删除数据库记录中属于该包的路径；共享文件保留，空目录自底向上清理。

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::hooks::{self, POST_REMOVE, PRE_REMOVE};
use crate::state::LocalDb;
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
    if !no_hooks {
        for hook in &hooks {
            if hook.phase == PRE_REMOVE {
                hooks::run_hook(PRE_REMOVE, package_name, &version, rootfs, &hook.script)?;
            }
        }
    }
    let mut report = RemoveReport::default();
    let mut directories = BTreeSet::new();

    for file in &files {
        let relative = sanitize_relative(Path::new(file))
            .map_err(|error| format!("数据库中的文件路径 `{file}` 非法: {error}"))?;

        if db.file_claimed_by_other(package_name, file)? {
            report.skipped_shared += 1;
            continue;
        }

        let target = rootfs.path().join(&relative);
        match fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    return Err(format!(
                        "数据库把目录 `{}` 记录成了文件，拒绝继续卸载",
                        target.display()
                    ));
                }
                fs::remove_file(&target)
                    .map_err(|error| format!("删除文件 `{}` 失败: {error}", target.display()))?;
                report.removed_files += 1;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                report.missing += 1;
            }
            Err(error) => return Err(format!("检查待删文件 `{}` 失败: {error}", target.display())),
        }

        collect_parent_directories(&relative, &mut directories);
    }

    report.removed_dirs = remove_empty_directories(rootfs.path(), &directories)?;

    if !no_hooks {
        for hook in &hooks {
            if hook.phase == POST_REMOVE {
                hooks::run_hook(POST_REMOVE, package_name, &version, rootfs, &hook.script)?;
            }
        }
    }

    // 只有文件系统、空目录和 post_remove 全部成功，才删除数据库记录。
    db.remove_package_records(package_name)?;
    Ok(report)
}

/// 收集文件路径的所有父目录，直到 Rootfs 根。
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

/// 由深到浅删除空目录；非空目录保留，避免误删其他包的内容。
fn remove_empty_directories(root: &Path, directories: &BTreeSet<PathBuf>) -> Result<usize, String> {
    let mut ordered = directories.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));

    let mut removed = 0;
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
        removed += 1;
    }
    Ok(removed)
}
