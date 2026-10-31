//! 外部发行版依赖到 xrpkg 内部 Capability 的映射。
//!
//! 配置文件是 `config/deps_map.toml`，结构为：
//! ```toml
//! [mappings]
//! "libssl3" = "openssl >= 3.0"
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::capability::Capability;

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 缺失配置文件时自动生成的模板。
const TEMPLATE: &str = r#"# xrpkg 外部依赖映射表
# 左边是 Debian/RPM/Arch 的依赖源名称，右边是 xrpkg 内部 Capability。
[mappings]
"libc6" = "glibc >= 2.44"
"libssl3" = "openssl >= 3.0"
"openssl-libs" = "openssl >= 3.0"
"libc.so.6" = "glibc >= 2.44"
"#;

/// 加载并查询外部依赖映射表。
#[derive(Debug, Default)]
pub struct DependencyMapper {
    mappings: BTreeMap<String, String>,
}

impl DependencyMapper {
    /// 从 TOML 文件加载映射；文件不存在时自动创建带示例的模板。
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("创建映射目录 `{}` 失败: {error}", parent.display())
                    })?;
                }
            }
            fs::write(path, TEMPLATE)
                .map_err(|error| format!("生成依赖映射模板 `{}` 失败: {error}", path.display()))?;
        }

        let text = fs::read_to_string(path)
            .map_err(|error| format!("读取依赖映射 `{}` 失败: {error}", path.display()))?;
        let document: MapDocument = toml::from_str(&text)
            .map_err(|error| format!("解析依赖映射 `{}` 失败: {error}", path.display()))?;
        Ok(Self {
            mappings: document.mappings,
        })
    }

    /// 把外部依赖字符串转换成映射表里的目标 Capability 字符串。
    ///
    /// 命中时返回映射值；未命中时原样返回，并打印 warning。
    pub fn map_dependency(&self, external_cap: &str) -> String {
        self.map_dependency_inner(external_cap, true)
    }

    /// 把外部依赖转换成内部 Capability；非法映射会回退到源包名。
    pub fn map_capability(&self, external_cap: &str) -> Result<Capability, Error> {
        self.map_capability_inner(external_cap, true)
    }

    /// 与 [`Self::map_capability`] 相同，但对“没有映射”保持安静。
    ///
    /// 同步整个外部仓库时，绝大多数依赖都不会出现在映射表里；逐条打印
    /// warning 只会淹没真正重要的跳过原因。
    pub fn map_capability_quiet(&self, external_cap: &str) -> Result<Capability, Error> {
        self.map_capability_inner(external_cap, false)
    }

    /// 返回映射值；`warn_unmapped` 控制未命中时是否打印 warning。
    fn map_dependency_inner(&self, external_cap: &str, warn_unmapped: bool) -> String {
        if let Some(mapped) = self.lookup(external_cap) {
            mapped.to_string()
        } else {
            if warn_unmapped {
                eprintln!("warning: 外部依赖 `{external_cap}` 没有映射，保留原依赖名");
            }
            external_cap.to_string()
        }
    }

    /// 把映射结果解析为 Capability；非法映射仍会打印配置错误 warning。
    fn map_capability_inner(
        &self,
        external_cap: &str,
        warn_unmapped: bool,
    ) -> Result<Capability, Error> {
        let mapped = self.map_dependency_inner(external_cap, warn_unmapped);
        match Capability::parse(&mapped) {
            Ok(capability) => Ok(capability),
            Err(error) => {
                let source_name = external_dependency_name(external_cap);
                if mapped != external_cap {
                    eprintln!(
                        "warning: 依赖 `{external_cap}` 的映射 `{mapped}` 不是合法 Capability（{error}），回退到源包名 `{source_name}`"
                    );
                }
                Capability::parse(source_name).map_err(|fallback_error| {
                    format!(
                        "依赖 `{external_cap}` 映射结果 `{mapped}` 非法，且回退源包名 `{source_name}` 也非法: {fallback_error}"
                    )
                })
            }
        }
    }

    /// 返回映射值，不做 warning。
    fn lookup(&self, external_cap: &str) -> Option<&str> {
        self.mappings
            .get(external_dependency_name(external_cap))
            .map(String::as_str)
    }
}

/// TOML 顶层结构。
#[derive(Debug, Deserialize)]
struct MapDocument {
    #[serde(default)]
    mappings: BTreeMap<String, String>,
}

/// 从外部依赖字符串中提取纯包名：
/// - `libssl3 (>= 1.1)` -> `libssl3`
/// - `glibc >= 2.17` -> `glibc`
/// - `libfoo>=1.0` -> `libfoo`
/// - `libc.so.6()(64bit)` -> `libc.so.6`
pub fn external_dependency_name(external_cap: &str) -> &str {
    let trimmed = external_cap.trim();
    let end = trimmed
        .find(|character: char| {
            character.is_whitespace() || matches!(character, '(' | '<' | '>' | '=')
        })
        .unwrap_or(trimmed.len());
    let name = &trimmed[..end];
    let name = name.strip_suffix(":amd64").unwrap_or(name);
    name
}

/// 配置文件路径辅助；当前项目根目录为 `config/deps_map.toml`。
pub fn default_config_path(project_root: &Path) -> PathBuf {
    project_root.join("config/deps_map.toml")
}
