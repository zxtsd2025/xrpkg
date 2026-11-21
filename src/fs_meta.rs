//! 文件元数据的读取、应用与复制。
//!
//! 包管理器不能只复制文件内容：SUID/SGID 位、属主和扩展属性（尤其是
//! `security.capability`）同样是包的一部分。本模块把 TAR/PAX 中的元数据
//! 归一化为 [`EntryMetadata`]，并在写盘、发布、备份恢复时统一应用。
//!
//! 非 root 环境下无法修改属主，也无法写入 `security.*` 属性；此时属主保持
//! 当前用户，但 `user.*` 等允许的属性仍会完整复制。真正的系统安装应以内核
//! 允许的 root 权限运行。

use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use base64::Engine;

/// 一个文件条目的完整元数据。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryMetadata {
    /// 完整权限位，包含 SUID(0o4000)、SGID(0o2000) 和 sticky(0o1000)。
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// 扩展属性；名字保留原始字节，值不做 UTF-8 转换。
    pub xattrs: Vec<(std::ffi::OsString, Vec<u8>)>,
}

impl EntryMetadata {
    /// 只有权限位的元数据，供没有 TAR 头的调用方使用。
    pub fn from_mode(mode: u32) -> Self {
        Self {
            mode: mode & 0o7777,
            ..Self::default()
        }
    }

    /// 从 TAR 条目读取权限、属主和 PAX 扩展属性。
    ///
    /// 必须在读取条目正文之前调用：PAX 扩展头附着在当前条目上。
    pub fn from_tar<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<Self, String> {
        let (mode, uid, gid) = {
            let header = entry.header();
            let mode = header.mode().unwrap_or(0o644) & 0o7777;
            let uid = header.uid().unwrap_or(0) as u32;
            let gid = header.gid().unwrap_or(0) as u32;
            (mode, uid, gid)
        };
        Ok(Self {
            mode,
            uid,
            gid,
            xattrs: read_tar_xattrs(entry)?,
        })
    }
}

/// 读取 TAR 条目上的扩展属性。
///
/// 兼容两种常见写法：
/// - GNU tar：`SCHILY.xattr.<name>=<raw bytes>`
/// - libarchive/bsdtar（Arch 包）：`LIBARCHIVE.xattr.<urlencoded name>=<base64>`
fn read_tar_xattrs<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>, String> {
    let Some(extensions) = entry
        .pax_extensions()
        .map_err(|error| format!("读取 TAR PAX 扩展失败: {error}"))?
    else {
        return Ok(Vec::new());
    };

    let mut xattrs = Vec::new();
    for extension in extensions {
        let extension = extension.map_err(|error| format!("解析 TAR PAX 扩展失败: {error}"))?;
        let key = extension.key_bytes();

        if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
            if name.is_empty() {
                continue;
            }
            xattrs.push((
                std::ffi::OsString::from_vec(name.to_vec()),
                extension.value_bytes().to_vec(),
            ));
        } else if let Some(name) = key.strip_prefix(b"LIBARCHIVE.xattr.") {
            let name =
                percent_decode(name).map_err(|error| format!("PAX 扩展属性名解码失败: {error}"))?;
            if name.is_empty() {
                continue;
            }
            let value = base64::engine::general_purpose::STANDARD
                .decode(extension.value_bytes())
                .map_err(|error| format!("PAX 扩展属性值不是合法 Base64: {error}"))?;
            xattrs.push((std::ffi::OsString::from_vec(name), value));
        }
    }
    Ok(xattrs)
}

/// 解析 libarchive 使用的百分号编码（例如 `security.capability` 中的点不需要转义）。
fn percent_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            let high = *input
                .get(index + 1)
                .ok_or_else(|| "百分号编码不完整".to_string())?;
            let low = *input
                .get(index + 2)
                .ok_or_else(|| "百分号编码不完整".to_string())?;
            let byte = (hex_value(high)? << 4) | hex_value(low)?;
            output.push(byte);
            index += 3;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }
    Ok(output)
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("非法十六进制字符 `{}`", byte as char)),
    }
}

/// 读取路径当前的完整元数据（不跟随符号链接）。
pub fn read_metadata(path: &Path) -> Result<EntryMetadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("读取元数据 `{}` 失败: {error}", path.display()))?;
    let xattrs = if metadata.file_type().is_symlink() {
        Vec::new()
    } else {
        read_xattrs(path)?
    };
    Ok(EntryMetadata {
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        xattrs,
    })
}

/// 列出路径上的全部扩展属性（不跟随符号链接）。
pub fn read_xattrs(path: &Path) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>, String> {
    let names = xattr::list_deref(path)
        .map_err(|error| format!("读取扩展属性列表 `{}` 失败: {error}", path.display()))?;
    let mut xattrs = Vec::new();
    for name in names {
        if let Some(value) = xattr::get_deref(path, &name)
            .map_err(|error| format!("读取扩展属性 `{}` 失败: {error}", path.display()))?
        {
            xattrs.push((name, value));
        }
    }
    Ok(xattrs)
}

/// 把元数据应用到普通文件或目录。
pub fn apply_metadata(path: &Path, metadata: &EntryMetadata) -> Result<(), String> {
    apply_metadata_inner(path, metadata, false)
}

/// 把元数据应用到符号链接本身（只处理属主；Linux 不允许 chmod 链接）。
pub fn apply_symlink_metadata(path: &Path, metadata: &EntryMetadata) -> Result<(), String> {
    apply_metadata_inner(path, metadata, true)
}

fn apply_metadata_inner(
    path: &Path,
    metadata: &EntryMetadata,
    symlink: bool,
) -> Result<(), String> {
    // 1. 属主。非 root 无法 chown，保持当前属主；root 下必须成功。
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        let result = if symlink {
            std::os::unix::fs::lchown(path, Some(metadata.uid), Some(metadata.gid))
        } else {
            std::os::unix::fs::chown(path, Some(metadata.uid), Some(metadata.gid))
        };
        result.map_err(|error| {
            format!(
                "设置 `{}` 的属主 {}:{} 失败: {error}",
                path.display(),
                metadata.uid,
                metadata.gid
            )
        })?;
    }

    // 2. 权限位。chown 可能清掉 SUID/SGID，因此必须在 chown 之后设置。
    if !symlink {
        fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode & 0o7777)).map_err(
            |error| {
                format!(
                    "设置 `{}` 权限 {:04o} 失败: {error}",
                    path.display(),
                    metadata.mode & 0o7777
                )
            },
        )?;
    }

    // 3. 扩展属性。root 下必须完整保留（包括 security.capability）。
    for (name, value) in &metadata.xattrs {
        let result = if symlink {
            xattr::set_deref(path, name, value)
        } else {
            xattr::set(path, name, value)
        };
        result.map_err(|error| {
            format!(
                "设置 `{}` 的扩展属性 `{}` 失败: {error}",
                path.display(),
                name.to_string_lossy()
            )
        })?;
    }

    Ok(())
}

/// 把源路径的元数据复制到目标路径（内容由调用方负责）。
pub fn copy_metadata(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = read_metadata(source)?;
    apply_metadata(destination, &metadata)
}

/// 判断当前进程是否具备 root 权限；测试用它决定是否覆盖 capability 场景。
pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_escaped_names() {
        assert_eq!(percent_decode(b"user.a%2Eb").unwrap(), b"user.a.b");
        assert!(percent_decode(b"user.%zz").is_err());
    }

    #[test]
    fn mode_keeps_special_bits() {
        assert_eq!(EntryMetadata::from_mode(0o104755).mode, 0o4755);
        assert_eq!(EntryMetadata::from_mode(0o102755).mode, 0o2755);
    }
}
