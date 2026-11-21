//! Red Hat `.rpm` 兼容轨道：元数据解析与安全文件提取。
//!
//! 使用成熟的 `rpm` crate 处理 RPM 头和 CPIO 载荷；本模块只做兼容轨道的
//! 元数据提取、文件落盘和路径安全，不把 RPM 依赖映射到 resolvo。

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use rpm::{FileType, Package, PackageMetadata};

use crate::capability::Capability;
use crate::deps_map::DependencyMapper;
use crate::fs_meta::EntryMetadata;
use crate::hooks::{HookScript, POST_INSTALL, POST_REMOVE, PRE_INSTALL, PRE_REMOVE};
use crate::xpkg::{ensure_directory, sanitize_relative, write_file, Rootfs};

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 从 RPM 包头提取出的最小元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpmMetadata {
    pub name: String,
    pub version: String,
    pub release: String,
    pub epoch: Option<u32>,
    /// 保持 RPM 原始依赖字符串，例如 `libc.so.6()(64bit)`。
    pub requires: Vec<String>,
}

impl RpmMetadata {
    /// 把 RPM Requires 翻译成内部 Capability；rpmlib 是 RPM 自身能力，跳过。
    pub fn mapped_requirements(&self, mapper: &DependencyMapper) -> Result<Vec<Capability>, Error> {
        self.requires
            .iter()
            .filter(|requirement| !requirement.starts_with("rpmlib("))
            .map(|requirement| mapper.map_capability(requirement))
            .collect()
    }
}

/// 读取 RPM 的 %pre/%post/%preun/%postun scriptlet。
pub fn read_rpm_hooks(rpm_path: &Path) -> Result<Vec<HookScript>, Error> {
    let metadata = PackageMetadata::open(rpm_path)
        .map_err(|error| format!("打开 RPM 元数据 `{}` 失败: {error}", rpm_path.display()))?;
    let mut hooks = Vec::new();
    for (phase, getter) in [
        (PRE_INSTALL, 0),
        (POST_INSTALL, 1),
        (PRE_REMOVE, 2),
        (POST_REMOVE, 3),
    ] {
        let scriptlet = match getter {
            0 => metadata.get_pre_install_script(),
            1 => metadata.get_post_install_script(),
            2 => metadata.get_pre_uninstall_script(),
            _ => metadata.get_post_uninstall_script(),
        };
        if let Ok(scriptlet) = scriptlet {
            if !scriptlet.script.trim().is_empty() {
                hooks.push(HookScript {
                    phase: phase.to_string(),
                    script: scriptlet.script.into_bytes(),
                });
            }
        }
    }
    Ok(hooks)
}

/// 读取 RPM 包头，提取包名、版本、release 和 Requires 字符串。
pub fn parse_rpm_metadata(rpm_path: &Path) -> Result<RpmMetadata, Error> {
    let metadata = PackageMetadata::open(rpm_path)
        .map_err(|error| format!("打开 RPM 元数据 `{}` 失败: {error}", rpm_path.display()))?;

    let name = metadata
        .get_name()
        .map_err(|error| format!("读取 RPM 包名失败: {error}"))?
        .to_string();
    let version = metadata
        .get_version()
        .map_err(|error| format!("读取 RPM 版本失败: {error}"))?
        .to_string();
    let release = metadata
        .get_release()
        .map_err(|error| format!("读取 RPM release 失败: {error}"))?
        .to_string();
    let epoch = metadata.get_epoch().ok();
    let requires = metadata
        .get_requires()
        .map_err(|error| format!("读取 RPM Requires 失败: {error}"))?
        .iter()
        .map(ToString::to_string)
        .collect();

    Ok(RpmMetadata {
        name,
        version,
        release,
        epoch,
        requires,
    })
}

/// 解包 RPM 的 CPIO 载荷到 target_rootfs，返回相对文件路径列表。
pub fn extract_rpm_data(rpm_path: &Path, target_rootfs: &Path) -> Result<Vec<PathBuf>, Error> {
    let package = Package::open(rpm_path)
        .map_err(|error| format!("打开 RPM `{}` 失败: {error}", rpm_path.display()))?;
    let rootfs = Rootfs::prepare(target_rootfs)?;

    let mut installed = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();

    for file in package
        .files()
        .map_err(|error| format!("读取 RPM 载荷失败: {error}"))?
    {
        let file = file.map_err(|error| format!("读取 RPM 文件条目失败: {error}"))?;
        let source_path = file.metadata.path();
        let normalized = normalize_rpm_path(&source_path);

        if normalized.as_os_str().is_empty() {
            if file.metadata.file_type() == FileType::Dir {
                continue;
            }
            return Err(format!(
                "RPM 路径 `{}` 不是合法的根目录条目",
                source_path.display()
            ));
        }

        let relative = sanitize_relative(&normalized)
            .map_err(|error| format!("RPM 路径 `{}` 非法: {error}", source_path.display()))?;

        if !seen_paths.insert(relative.clone()) {
            return Err(format!("RPM 出现重复路径 `{}`", source_path.display()));
        }

        match file.metadata.file_type() {
            FileType::Dir => ensure_directory(rootfs.path(), &relative)?,
            FileType::Regular => {
                let Some(content) = file.content() else {
                    // Ghost 文件只在头里存在，没有载荷内容，不落盘。
                    continue;
                };
                let metadata = EntryMetadata::from_mode(file.metadata.permissions() as u32);
                let mut reader = Cursor::new(content);
                write_file(
                    rootfs.path(),
                    &relative,
                    &mut reader,
                    content.len() as u64,
                    &metadata,
                )?;
                installed.insert(relative);
            }
            FileType::SymbolicLink => {
                return Err(format!(
                    "拒绝 RPM 中的符号链接 `{}`，防止链接逃逸 Rootfs",
                    source_path.display()
                ));
            }
            other => {
                return Err(format!(
                    "拒绝 RPM 中的非普通文件/目录条目 `{}`（类型 {other:?}）",
                    source_path.display()
                ))
            }
        }
    }

    Ok(installed.into_iter().collect())
}

/// RPM 头里的路径通常是 `/usr/...`；去掉一个根前缀后再做组件安全检查。
fn normalize_rpm_path(path: &Path) -> PathBuf {
    let relative = path.strip_prefix("/").unwrap_or(path);
    let relative = relative.strip_prefix(".").unwrap_or(relative);
    relative.to_path_buf()
}
