//! ABI 守门员：读取 ELF 的 GNU 符号版本需求，拦截超过 GLIBC 基线的文件。
//!
//! 当前基线固定为 [`MAX_GLIBC_VERSION`]。只检查 ELF 文件；非 ELF 文件直接跳过。

use std::fs;
use std::path::Path;

use goblin::elf::Elf;

use crate::capability::Version;

/// 本系统允许的最高 GLIBC 符号版本。
pub const MAX_GLIBC_VERSION: &str = "2.44";

/// 负责比较 ELF 所需 GLIBC 版本与系统基线。
#[derive(Clone, Debug)]
pub struct AbiGuard {
    max_glibc: Version,
}

impl AbiGuard {
    /// 用指定基线创建守门员。
    pub fn new(max_glibc: &str) -> Result<Self, String> {
        let max_glibc =
            Version::parse(max_glibc).map_err(|error| format!("ABI 基线版本无效: {error}"))?;
        Ok(Self { max_glibc })
    }

    /// 返回当前 GLIBC 基线版本。
    pub fn max_glibc(&self) -> &Version {
        &self.max_glibc
    }

    /// 检查一个已经解包到暂存目录的 ELF 文件。
    ///
    /// `relative_path` 是相对 Rootfs 的路径，仅用于报错，例如 `bin/app`。
    pub fn check_file(
        &self,
        package_name: &str,
        relative_path: &str,
        staged_file: &Path,
    ) -> Result<(), String> {
        let Some(required) = inspect_elf(staged_file)? else {
            return Ok(());
        };

        if required > self.max_glibc {
            return Err(format!(
                "包 {package_name} 中的 /{relative_path} 需要 GLIBC_{required}，但系统基线为 {}",
                self.max_glibc
            ));
        }

        Ok(())
    }
}

/// 读取 ELF 的 `.gnu.version_r`，返回最高的 GLIBC 版本需求。
///
/// - 非 ELF 文件返回 `Ok(None)`；
/// - 有 ELF 魔数但解析失败返回 `Err`，避免把损坏文件当成安全文件；
/// - `GLIBC_PRIVATE` 这类非数字版本会被忽略。
pub fn inspect_elf(path: &Path) -> Result<Option<Version>, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("读取 ELF 文件 `{}` 失败: {error}", path.display()))?;

    if !bytes.starts_with(b"\x7fELF") {
        return Ok(None);
    }

    let elf = Elf::parse(&bytes)
        .map_err(|error| format!("解析 ELF 文件 `{}` 失败: {error}", path.display()))?;

    let mut highest = None;
    if let Some(verneed) = &elf.verneed {
        for need in verneed.iter() {
            for auxiliary in need.iter() {
                let Some(name) = elf.dynstrtab.get_at(auxiliary.vna_name) else {
                    continue;
                };
                let Some(version_text) = name.strip_prefix("GLIBC_") else {
                    continue;
                };
                let Ok(version) = Version::parse(version_text) else {
                    continue;
                };

                if highest
                    .as_ref()
                    .is_none_or(|current: &Version| version > *current)
                {
                    highest = Some(version);
                }
            }
        }
    }

    Ok(highest)
}
