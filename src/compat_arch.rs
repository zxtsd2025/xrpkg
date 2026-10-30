//! Arch Linux `.pkg.tar.zst` 兼容轨道。
//!
//! Arch 包就是 ZSTD 压缩的 TAR；根目录的 `.PKGINFO` 是纯文本键值元数据。
//! 本模块只做元数据解析、安全文件提取和 ABI 前置检查。

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::capability::Capability;
use crate::deps_map::DependencyMapper;
use crate::hooks::{HookScript, POST_INSTALL, POST_REMOVE, PRE_INSTALL, PRE_REMOVE};
use crate::xpkg::{ensure_directory, sanitize_relative, write_file, Rootfs};

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 从 `.PKGINFO` 提取出的最小元数据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchMetadata {
    pub pkgname: String,
    pub pkgver: String,
    pub depends: Vec<String>,
}

impl ArchMetadata {
    /// 把 Arch depend 翻译成内部 Capability。
    pub fn mapped_requirements(&self, mapper: &DependencyMapper) -> Result<Vec<Capability>, Error> {
        self.depends
            .iter()
            .map(|dependency| mapper.map_capability(dependency))
            .collect()
    }
}

/// 读取 Arch `.INSTALL`，为四个阶段生成函数调用包装脚本。
pub fn read_arch_hooks(arch_path: &Path) -> Result<Vec<HookScript>, Error> {
    let file = File::open(arch_path)
        .map_err(|error| format!("打开 Arch 包 `{}` 失败: {error}", arch_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    let mut archive = tar::Archive::new(decoder);
    let mut install_script = None;
    for entry in archive
        .entries()
        .map_err(|error| format!("读取 Arch TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 Arch TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 Arch TAR 路径失败: {error}"))?
            .into_owned();
        if normalize_entry_path(&path) != Path::new(".INSTALL") {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(".INSTALL 必须是普通文件".to_string());
        }
        if entry.size() > 1024 * 1024 {
            return Err(".INSTALL 超过 1 MiB".to_string());
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| format!("读取 .INSTALL 失败: {error}"))?;
        install_script = Some(text);
    }

    let Some(install_script) = install_script else {
        return Ok(Vec::new());
    };
    let mut hooks = Vec::new();
    for phase in [PRE_INSTALL, POST_INSTALL, PRE_REMOVE, POST_REMOVE] {
        let script = format!(
            "{install_script}\nif declare -F {phase} >/dev/null 2>&1; then\n  {phase}\nfi\n"
        );
        hooks.push(HookScript {
            phase: phase.to_string(),
            script: script.into_bytes(),
        });
    }
    Ok(hooks)
}

/// 只读 `.PKGINFO`，找到后立即停止，不遍历后续文件。
pub fn parse_arch_metadata(arch_path: &Path) -> Result<ArchMetadata, Error> {
    let file = File::open(arch_path)
        .map_err(|error| format!("打开 Arch 包 `{}` 失败: {error}", arch_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    parse_pkg_info(decoder)
}

/// 安全解包 Arch tar.zst 到 target_rootfs，跳过根目录点文件元数据。
pub fn extract_arch_data(arch_path: &Path, target_rootfs: &Path) -> Result<Vec<PathBuf>, Error> {
    let file = File::open(arch_path)
        .map_err(|error| format!("打开 Arch 包 `{}` 失败: {error}", arch_path.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    let rootfs = Rootfs::prepare(target_rootfs)?;
    let mut archive = tar::Archive::new(decoder);

    let mut installed = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();

    for entry in archive
        .entries()
        .map_err(|error| format!("读取 Arch TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 Arch TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 Arch TAR 条目路径失败: {error}"))?
            .into_owned();
        let normalized = normalize_entry_path(&path);

        if is_root_dot_metadata(&normalized) {
            continue;
        }
        if normalized.as_os_str().is_empty() {
            if entry.header().entry_type().is_dir() {
                continue;
            }
            return Err(format!(
                "Arch 包路径 `{}` 不是合法的根目录条目",
                path.display()
            ));
        }

        let relative = sanitize_relative(&normalized)
            .map_err(|error| format!("Arch 包路径 `{}` 非法: {error}", path.display()))?;

        if !seen_paths.insert(relative.clone()) {
            return Err(format!("Arch 包出现重复路径 `{}`", path.display()));
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            ensure_directory(rootfs.path(), &relative)?;
        } else if entry_type.is_file() {
            let size = entry.size();
            let mode = entry.header().mode().unwrap_or(0o644);
            write_file(rootfs.path(), &relative, &mut entry, size, mode)?;
            installed.insert(relative);
        } else if entry_type.is_symlink() {
            return Err(format!(
                "拒绝 Arch 包中的符号链接 `{}`，防止链接逃逸 Rootfs",
                path.display()
            ));
        } else {
            return Err(format!(
                "拒绝 Arch 包中的非普通文件/目录条目 `{}`",
                path.display()
            ));
        }
    }

    Ok(installed.into_iter().collect())
}

/// 从任意 TAR 读取器查找 `.PKGINFO`；找到就返回，不读取剩余内容。
fn parse_pkg_info<R: Read>(reader: R) -> Result<ArchMetadata, Error> {
    let mut archive = tar::Archive::new(reader);

    for entry in archive
        .entries()
        .map_err(|error| format!("读取 Arch TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 Arch TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 Arch TAR 条目路径失败: {error}"))?
            .into_owned();
        if normalize_entry_path(&path) != Path::new(".PKGINFO") {
            continue;
        }

        if !entry.header().entry_type().is_file() {
            return Err(".PKGINFO 必须是普通文件".to_string());
        }
        if entry.size() > 1024 * 1024 {
            return Err(".PKGINFO 超过 1 MiB 上限".to_string());
        }

        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| format!("读取 .PKGINFO 失败: {error}"))?;
        return parse_pkg_info_text(&text);
    }

    Err("Arch 包缺少 .PKGINFO".to_string())
}

/// 解析 `.PKGINFO` 的 `key = value` 行；`depend` 可以重复出现。
fn parse_pkg_info_text(text: &str) -> Result<ArchMetadata, Error> {
    let mut pkgname = None;
    let mut pkgver = None;
    let mut depends = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "pkgname" => pkgname = Some(value.to_string()),
            "pkgver" => pkgver = Some(value.to_string()),
            "depend" => depends.push(value.to_string()),
            _ => {}
        }
    }

    let pkgname = pkgname
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ".PKGINFO 缺少 pkgname".to_string())?;
    let pkgver = pkgver
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ".PKGINFO 缺少 pkgver".to_string())?;

    Ok(ArchMetadata {
        pkgname,
        pkgver,
        depends,
    })
}

/// Arch 路径通常以 `./` 开头，去掉这个前缀后再做安全检查。
fn normalize_entry_path(path: &Path) -> PathBuf {
    path.strip_prefix(".").unwrap_or(path).to_path_buf()
}

/// 只跳过根目录下以点开头的元数据文件，例如 .PKGINFO、.INSTALL、.MTREE。
fn is_root_dot_metadata(path: &Path) -> bool {
    let mut components = path.components();
    let Some(Component::Normal(name)) = components.next() else {
        return false;
    };
    name.to_string_lossy().starts_with('.') && components.next().is_none()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    struct CountingReader<R> {
        inner: R,
        bytes: usize,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.inner.read(buffer)?;
            self.bytes += count;
            Ok(count)
        }
    }

    fn append(builder: &mut tar::Builder<Vec<u8>>, path: &str, contents: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        builder.append_data(&mut header, path, contents).unwrap();
    }

    #[test]
    fn pkg_info_parsing_stops_after_the_metadata_entry() {
        let mut builder = tar::Builder::new(Vec::new());
        append(
            &mut builder,
            ".PKGINFO",
            b"pkgname = stream-test\npkgver = 1.0-1\ndepend = glibc\n",
            0o644,
        );
        append(
            &mut builder,
            "usr/share/stream-test/big.bin",
            &vec![0u8; 8 * 1024 * 1024],
            0o644,
        );
        let tar = builder.into_inner().unwrap();
        let total = tar.len();
        let mut reader = CountingReader {
            inner: Cursor::new(tar),
            bytes: 0,
        };

        let metadata = parse_pkg_info(&mut reader).unwrap();

        assert_eq!(metadata.pkgname, "stream-test");
        assert!(
            reader.bytes < 4096,
            "只应读取元数据，实际读取 {} / {} 字节",
            reader.bytes,
            total
        );
    }
}
