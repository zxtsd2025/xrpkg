//! 从外部 TOML 文件加载包场景和安装仓库。
//!
//! TOML 只负责提供原始字符串；版本号和 Capability 的合法性由本模块统一转换时校验。

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::capability::{Capability, Version};
use crate::solver::Package;

/// 一个可直接交给求解器的场景。
#[derive(Clone, Debug)]
pub struct Scenario {
    pub title: String,
    pub root: String,
    pub packages: Vec<Package>,
}

/// 读取并校验 TOML 文件，返回其中全部演示场景。
pub fn load_scenarios(path: impl AsRef<Path>) -> Result<Vec<Scenario>, String> {
    let path = path.as_ref();
    let document = read_document(path)?;

    if document.scenarios.is_empty() {
        return Err(format!("`{}` 中没有定义任何场景", path.display()));
    }

    document
        .scenarios
        .into_iter()
        .map(ScenarioDocument::into_scenario)
        .collect()
}

/// 读取并校验 TOML 文件，返回 install 使用的可用仓库包列表。
pub fn load_repository(path: impl AsRef<Path>) -> Result<Vec<Package>, String> {
    let path = path.as_ref();
    let document = read_document(path)?;

    if document.repository.is_empty() {
        return Err(format!("`{}` 中没有定义任何仓库包", path.display()));
    }

    document
        .repository
        .into_iter()
        .map(PackageDocument::into_package)
        .collect()
}

/// 读取并解析 TOML 顶层结构。
fn read_document(path: &Path) -> Result<PackagesDocument, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("读取 `{}` 失败: {error}", path.display()))?;
    toml::from_str(&source).map_err(|error| format!("解析 `{}` 失败: {error}", path.display()))
}

/// TOML 顶层结构。
#[derive(Debug, Deserialize)]
struct PackagesDocument {
    scenarios: Vec<ScenarioDocument>,
    #[serde(default)]
    repository: Vec<PackageDocument>,
}

/// TOML 中单个场景的原始结构。
#[derive(Debug, Deserialize)]
struct ScenarioDocument {
    title: String,
    root: String,
    packages: Vec<PackageDocument>,
}

/// TOML 中单个包版本的原始结构；requires 是待解析的 Capability 字符串。
#[derive(Debug, Deserialize)]
struct PackageDocument {
    name: String,
    version: String,
    #[serde(default)]
    requires: Vec<String>,
}

impl ScenarioDocument {
    /// 把原始场景转换为求解器模型，并把错误补上场景标题。
    fn into_scenario(self) -> Result<Scenario, String> {
        let Self {
            title,
            root,
            packages,
        } = self;

        let packages = packages
            .into_iter()
            .map(PackageDocument::into_package)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("场景 `{title}`: {error}"))?;

        Ok(Scenario {
            title,
            root,
            packages,
        })
    }
}

impl PackageDocument {
    /// 把原始包转换为 Package；这里统一解析版本号和每条 Capability。
    fn into_package(self) -> Result<Package, String> {
        let Self {
            name,
            version,
            requires,
        } = self;

        let version = Version::parse(&version)
            .map_err(|error| format!("包 `{name}` 的 version 字段无效: {error}"))?;
        let requires = requires
            .into_iter()
            .map(|text| {
                Capability::parse(&text)
                    .map_err(|error| format!("包 `{name}` 的 requires 项 `{text}` 无效: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Package::new(name, version, requires))
    }
}
