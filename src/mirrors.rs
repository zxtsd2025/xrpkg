//! Arch 镜像源配置：`config/mirrors.toml`。
//!
//! 每一项的 `url` 指向一个包含 `core.db` 的仓库目录，例如：
//! `https://mirrors.tuna.tsinghua.edu.cn/archlinux/core/os/x86_64/`。
//! `priority` 越小优先级越高；下载失败时按优先级依次回退。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 一个可用的 Arch 镜像仓库。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mirror {
    pub name: String,
    pub url: String,
    pub priority: i64,
}

/// 默认配置文件路径：`config/mirrors.toml`。
pub fn default_config_path(project_root: &Path) -> PathBuf {
    project_root.join("config/mirrors.toml")
}

/// 读取并按优先级排序镜像源；没有配置或重名都会报错。
pub fn load(path: &Path) -> Result<Vec<Mirror>, Error> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("读取镜像配置 `{}` 失败: {error}", path.display()))?;
    let document: MirrorDocument = toml::from_str(&text)
        .map_err(|error| format!("解析镜像配置 `{}` 失败: {error}", path.display()))?;
    if document.mirrors.is_empty() {
        return Err(format!(
            "镜像配置 `{}` 中没有任何 [[mirrors]]",
            path.display()
        ));
    }

    let mut names = BTreeSet::new();
    let mut mirrors = Vec::with_capacity(document.mirrors.len());
    for entry in document.mirrors {
        validate_name(&entry.name)?;
        if entry.url.trim().is_empty() || entry.url.chars().any(char::is_whitespace) {
            return Err(format!("镜像 `{}` 的 url 不能为空或包含空白", entry.name));
        }
        if !names.insert(entry.name.clone()) {
            return Err(format!("镜像配置中出现重复名称 `{}`", entry.name));
        }
        mirrors.push(Mirror {
            name: entry.name,
            url: entry.url,
            priority: entry.priority,
        });
    }

    mirrors.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(mirrors)
}

/// 按名称查找镜像。
pub fn find<'a>(mirrors: &'a [Mirror], name: &str) -> Result<&'a Mirror, Error> {
    mirrors
        .iter()
        .find(|mirror| mirror.name == name)
        .ok_or_else(|| {
            format!(
                "镜像配置中没有 `{name}`；可用镜像: {}",
                mirrors
                    .iter()
                    .map(|mirror| mirror.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// 把 `sync` 的参数解析成实际仓库 URL：
/// - `http(s)://...`、`file://...`、绝对/相对路径直接使用；
/// - 其它字符串按镜像名查表。
pub fn resolve_target(mirrors: &[Mirror], target: &str) -> Result<String, Error> {
    if is_direct_location(target) {
        Ok(target.to_string())
    } else {
        find(mirrors, target).map(|mirror| mirror.url.clone())
    }
}

/// 判断参数是直接 URL/路径，而不是镜像名。
pub fn is_direct_location(target: &str) -> bool {
    target.contains("://")
        || Path::new(target).is_absolute()
        || target.starts_with("./")
        || target.starts_with("../")
        || Path::new(target).exists()
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
    {
        return Err(format!("镜像名 `{name}` 非法"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MirrorDocument {
    #[serde(default)]
    mirrors: Vec<MirrorEntry>,
}

#[derive(Debug, Deserialize)]
struct MirrorEntry {
    name: String,
    url: String,
    #[serde(default)]
    priority: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("mirrors.toml");
        fs::write(&path, contents).unwrap();
        (temp, path)
    }

    #[test]
    fn sorts_mirrors_by_priority_then_name() {
        let (_temp, path) = write_config(
            "[[mirrors]]\nname = \"slow\"\nurl = \"https://slow.invalid/\"\npriority = 9\n\n[[mirrors]]\nname = \"fast\"\nurl = \"https://fast.invalid/\"\npriority = 1\n",
        );

        let mirrors = load(&path).unwrap();

        assert_eq!(mirrors[0].name, "fast");
        assert_eq!(mirrors[1].name, "slow");
    }

    #[test]
    fn rejects_duplicate_names() {
        let (_temp, path) = write_config(
            "[[mirrors]]\nname = \"tuna\"\nurl = \"https://a.invalid/\"\n\n[[mirrors]]\nname = \"tuna\"\nurl = \"https://b.invalid/\"\n",
        );

        let error = load(&path).unwrap_err();

        assert!(error.contains("重复名称"), "{error}");
    }

    #[test]
    fn resolves_names_and_direct_locations() {
        let mirrors = vec![Mirror {
            name: "tuna".to_string(),
            url: "https://tuna.invalid/repo/".to_string(),
            priority: 1,
        }];

        assert_eq!(
            resolve_target(&mirrors, "tuna").unwrap(),
            "https://tuna.invalid/repo/"
        );
        assert_eq!(
            resolve_target(&mirrors, "https://other.invalid/repo/").unwrap(),
            "https://other.invalid/repo/"
        );
    }
}
