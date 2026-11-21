//! Debian `.deb` 兼容轨道：格式解析与安全文件提取。
//!
//! `.deb` 是 ar 归档，通常包含：
//! - `debian-binary`
//! - `control.tar.{gz,xz,zst}`
//! - `data.tar.{gz,xz,zst}`
//!
//! 本阶段只解析元数据和提取文件，不把 Debian 的复杂依赖语法映射到 resolvo。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use deb822_lossless::Deb822;
use flate2::read::GzDecoder;
use liblzma::read::XzDecoder;

use crate::capability::Capability;
use crate::deps_map::DependencyMapper;
use crate::fs_meta::EntryMetadata;
use crate::hooks::{HookScript, POST_INSTALL, POST_REMOVE, PRE_INSTALL, PRE_REMOVE};
use crate::xpkg::{ensure_directory_with_metadata, sanitize_relative, write_file, Rootfs};

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 从 Debian control 文件提取出的最小元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebMetadata {
    pub package: String,
    pub version: String,
    pub depends: Option<String>,
}

impl DebMetadata {
    /// 把 Debian Depends 翻译成内部 Capability。
    ///
    /// 逗号表示 AND；`|` 的替代依赖本阶段取第一个可映射项。
    pub fn mapped_requirements(&self, mapper: &DependencyMapper) -> Result<Vec<Capability>, Error> {
        let Some(depends) = &self.depends else {
            return Ok(Vec::new());
        };

        let mut requirements = Vec::new();
        for group in depends.split(',') {
            let Some(first_alternative) = group.split('|').next() else {
                continue;
            };
            let first_alternative = first_alternative.trim();
            if first_alternative.is_empty() {
                continue;
            }
            requirements.push(mapper.map_capability(first_alternative)?);
        }
        Ok(requirements)
    }
}

/// 读取 Debian control.tar 中的 preinst/postinst/prerm/postrm。
pub fn read_deb_hooks(deb_path: &Path) -> Result<Vec<HookScript>, Error> {
    let members = read_deb_members(deb_path)?;
    let mut archive = tar::Archive::new(member_reader(
        &members.control_name,
        members.control.clone(),
    )?);
    let mut hooks = Vec::new();
    for entry in archive
        .entries()
        .map_err(|error| format!("读取 control.tar 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 control.tar 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 control.tar 条目路径失败: {error}"))?
            .into_owned();
        let name = strip_leading_dot(&path);
        let phase = match name.to_str() {
            Some("preinst") => PRE_INSTALL,
            Some("postinst") => POST_INSTALL,
            Some("prerm") => PRE_REMOVE,
            Some("postrm") => POST_REMOVE,
            _ => continue,
        };
        if !entry.header().entry_type().is_file() {
            return Err(format!("Debian hook `{phase}` 必须是普通文件"));
        }
        if entry.size() > 1024 * 1024 {
            return Err(format!("Debian hook `{phase}` 超过 1 MiB"));
        }
        let mut script = Vec::new();
        entry
            .read_to_end(&mut script)
            .map_err(|error| format!("读取 Debian hook `{phase}` 失败: {error}"))?;
        hooks.push(HookScript {
            phase: phase.to_string(),
            script,
        });
    }
    Ok(hooks)
}

/// 解析 control.tar.* 中的 control 文件。
pub fn parse_deb_metadata(deb_path: &Path) -> Result<DebMetadata, Error> {
    let members = read_deb_members(deb_path)?;
    let control = read_control(&members)?;
    parse_control(&control)
}

/// 把 data.tar.* 安全解包到 target_rootfs，返回相对文件路径列表。
pub fn extract_deb_data(deb_path: &Path, target_rootfs: &Path) -> Result<Vec<PathBuf>, Error> {
    let members = read_deb_members(deb_path)?;
    let rootfs = Rootfs::prepare(target_rootfs)?;
    let mut archive = tar::Archive::new(member_reader(&members.data_name, members.data)?);

    let mut installed = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();

    for entry in archive
        .entries()
        .map_err(|error| format!("读取 data.tar 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 data.tar 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 data.tar 条目路径失败: {error}"))?
            .into_owned();
        let normalized = strip_leading_dot(&path);
        let entry_type = entry.header().entry_type();

        if normalized.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err(format!(
                "data.tar 路径 `{}` 不是合法的根目录条目",
                path.display()
            ));
        }

        let relative = sanitize_relative(normalized)
            .map_err(|error| format!("data.tar 路径 `{}` 非法: {error}", path.display()))?;

        if !seen_paths.insert(relative.clone()) {
            return Err(format!("data.tar 出现重复路径 `{}`", path.display()));
        }

        if entry_type.is_dir() {
            let metadata = EntryMetadata::from_tar(&mut entry)?;
            ensure_directory_with_metadata(rootfs.path(), &relative, &metadata)?;
        } else if entry_type.is_file() {
            let size = entry.size();
            let metadata = EntryMetadata::from_tar(&mut entry)?;
            write_file(rootfs.path(), &relative, &mut entry, size, &metadata)?;
            installed.insert(relative);
        } else if entry_type.is_symlink() {
            return Err(format!(
                "拒绝 data.tar 中的符号链接 `{}`，防止通过链接逃逸 Rootfs",
                path.display()
            ));
        } else {
            return Err(format!(
                "拒绝 data.tar 中的非普通文件/目录条目 `{}`",
                path.display()
            ));
        }
    }

    Ok(installed.into_iter().collect())
}

/// ar 归档中与解析有关的三类成员。
struct DebMembers {
    control_name: String,
    control: Vec<u8>,
    data_name: String,
    data: Vec<u8>,
}

/// 读取 .deb 的 ar 归档，提取 debian-binary、control.tar.* 和 data.tar.*。
fn read_deb_members(deb_path: &Path) -> Result<DebMembers, Error> {
    let file = File::open(deb_path)
        .map_err(|error| format!("打开 .deb `{}` 失败: {error}", deb_path.display()))?;
    let mut archive = ar::Archive::new(file);

    let mut debian_binary = None;
    let mut control = None;
    let mut data = None;

    while let Some(entry) = archive.next_entry() {
        let mut entry = entry.map_err(|error| format!("读取 ar 成员失败: {error}"))?;
        let identifier = entry.header().identifier();
        let name = String::from_utf8_lossy(identifier)
            .trim_end_matches('/')
            .to_string();

        if name == "debian-binary" {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| format!("读取 debian-binary 失败: {error}"))?;
            debian_binary = Some(bytes);
        } else if name.starts_with("control.tar.") {
            if control.is_some() {
                return Err("包内出现多个 control.tar.*".to_string());
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| format!("读取 {name} 失败: {error}"))?;
            control = Some((name, bytes));
        } else if name.starts_with("data.tar.") {
            if data.is_some() {
                return Err("包内出现多个 data.tar.*".to_string());
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .map_err(|error| format!("读取 {name} 失败: {error}"))?;
            data = Some((name, bytes));
        }
    }

    let version = debian_binary.ok_or_else(|| "不是有效的 .deb：缺少 debian-binary".to_string())?;
    let version = String::from_utf8_lossy(&version);
    if version.trim() != "2.0" {
        return Err(format!("不支持的 debian-binary 版本 `{}`", version.trim()));
    }

    let (control_name, control) =
        control.ok_or_else(|| "不是有效的 .deb：缺少 control.tar.*".to_string())?;
    let (data_name, data) = data.ok_or_else(|| "不是有效的 .deb：缺少 data.tar.*".to_string())?;

    Ok(DebMembers {
        control_name,
        control,
        data_name,
        data,
    })
}

/// 读取 control.tar.* 中的 control 文件内容。
fn read_control(members: &DebMembers) -> Result<String, Error> {
    let reader = member_reader(&members.control_name, members.control.clone())?;
    let mut archive = tar::Archive::new(reader);
    let mut control = None;

    for entry in archive
        .entries()
        .map_err(|error| format!("读取 control.tar 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 control.tar 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 control.tar 条目路径失败: {error}"))?
            .into_owned();

        if strip_leading_dot(&path) != Path::new("control") {
            continue;
        }
        if control.is_some() {
            return Err("control.tar 中出现多个 control 文件".to_string());
        }
        if !entry.header().entry_type().is_file() {
            return Err("control 必须是普通文件".to_string());
        }
        if entry.size() > 1024 * 1024 {
            return Err("control 文件超过 1 MiB 上限".to_string());
        }

        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| format!("读取 control 文件失败: {error}"))?;
        control = Some(text);
    }

    control.ok_or_else(|| "control.tar 中缺少 control 文件".to_string())
}

/// 解析 Debian Deb822 control 文本，只取 Package、Version、Depends。
fn parse_control(text: &str) -> Result<DebMetadata, Error> {
    let document =
        Deb822::from_str(text).map_err(|error| format!("解析 Debian control 失败: {error}"))?;
    let paragraph = document
        .paragraphs()
        .next()
        .ok_or_else(|| "Debian control 中没有段落".to_string())?;

    let package = paragraph
        .get("Package")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Debian control 缺少 Package 字段".to_string())?;
    let version = paragraph
        .get("Version")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Debian control 缺少 Version 字段".to_string())?;
    let depends = paragraph
        .get("Depends")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    Ok(DebMetadata {
        package,
        version,
        depends,
    })
}

/// 根据成员扩展名选择 gzip、xz、zstd 或未压缩 TAR 读取器。
fn member_reader(name: &str, bytes: Vec<u8>) -> Result<Box<dyn Read>, Error> {
    let cursor = Cursor::new(bytes);
    if name.ends_with(".gz") {
        Ok(Box::new(GzDecoder::new(cursor)))
    } else if name.ends_with(".xz") {
        Ok(Box::new(XzDecoder::new(cursor)))
    } else if name.ends_with(".zst") {
        let decoder = zstd::stream::read::Decoder::new(cursor)
            .map_err(|error| format!("创建 {name} 的 ZSTD 解码器失败: {error}"))?;
        Ok(Box::new(decoder))
    } else if name.ends_with(".tar") {
        Ok(Box::new(cursor))
    } else {
        Err(format!(
            "不支持的压缩格式 `{name}`；支持 .tar、.tar.gz、.tar.xz、.tar.zst"
        ))
    }
}

/// Debian data.tar 的路径通常以 `./` 开头，先去掉了这一个前缀。
fn strip_leading_dot(path: &Path) -> &Path {
    path.strip_prefix(".").unwrap_or(path)
}
