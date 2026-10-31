//! 全系统构建编排：按 `recipes/system.toml` 的既定顺序逐个构建并安装。
//!
//! 编排器不重新计算依赖顺序；配置文件中的顺序就是唯一事实来源。
//! 单个包构建失败或安装失败时立即停止，后续包不会被处理；已成功产出的
//! `.xpkg` 保留在 `repo/`，下一次运行会直接复用，从而实现断点续传。

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::builder::{self, BuildOptions};
use crate::capability::Version;
use crate::installer;
use crate::solver::SelectedPackage;
use crate::state::LocalDb;
use crate::xpkg;

/// `recipes/system.toml` 的结构；数组顺序即构建顺序。
#[derive(Debug, Deserialize)]
struct SystemConfig {
    recipes: Vec<String>,
}

/// build-world 的执行选项。
#[derive(Clone, Debug)]
pub struct WorldOptions {
    /// 是否在 unshare + chroot 沙箱中执行每个配方的 build/package。
    pub sandbox: bool,
    /// 传给每个构建的 MAKEFLAGS `-jN`。
    pub jobs: Option<usize>,
    /// 是否跳过安装 hooks。
    pub no_hooks: bool,
    /// 编排日志路径；CLI 默认使用 `~/.cache/xrpkg/logs/build-world.log`。
    pub log_path: PathBuf,
}

/// 一次 build-world 的统计结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldReport {
    /// 本次真正执行了构建的包数。
    pub built: usize,
    /// 因 `repo/<name>-<version>.xpkg` 已存在而跳过构建的包数。
    pub skipped: usize,
    /// 本次新安装（或升级）的包数。
    pub installed: usize,
    /// 数据库中已有相同版本、因而跳过安装的包数。
    pub already_installed: usize,
}

/// 同时向终端和 `build-world.log` 写一行；日志失败不覆盖真正的构建错误。
struct WorldLog {
    file: File,
}

impl WorldLog {
    /// 创建（或截断）日志文件，并确保父目录存在。
    fn create(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "创建 build-world 日志目录 `{}` 失败: {error}",
                        parent.display()
                    )
                })?;
            }
        }
        let file = File::create(path)
            .map_err(|error| format!("创建 build-world 日志 `{}` 失败: {error}", path.display()))?;
        Ok(Self { file })
    }

    /// 写一行日志；终端输出与文件内容保持一致。
    fn line(&mut self, message: &str) {
        println!("{message}");
        let _ = writeln!(self.file, "{message}");
        let _ = self.file.flush();
    }
}

/// 按系统清单顺序构建并安装全部配方；第一个失败会立即终止后续包。
pub fn build_world(
    config_path: &Path,
    repo_dir: &Path,
    rootfs_path: &Path,
    db_path: &Path,
    options: &WorldOptions,
) -> Result<WorldReport, String> {
    let mut log = WorldLog::create(&options.log_path)?;
    log.line("==> build-world 开始");
    log.line(&format!("==> 系统清单: {}", config_path.display()));

    let recipes = match load_recipes(config_path) {
        Ok(recipes) => recipes,
        Err(error) => {
            log.line(&format!("==> 读取系统清单失败: {error}"));
            return Err(error);
        }
    };
    log.line(&format!(
        "==> 共 {} 个包；输出仓库 {}；目标 Rootfs {}",
        recipes.len(),
        repo_dir.display(),
        rootfs_path.display()
    ));
    log.line(&format!(
        "==> 数据库: {}；串行编排，--jobs 仅传给每个构建的 MAKEFLAGS",
        db_path.display()
    ));

    let mut db = LocalDb::open(db_path)?;
    let mut report = WorldReport::default();

    for (index, recipe_path) in recipes.iter().enumerate() {
        let identity = match builder::read_recipe_identity(recipe_path) {
            Ok(identity) => identity,
            Err(error) => {
                let message = format!(
                    "build-world 读取配方 `{}` 失败，后续包未处理: {error}",
                    recipe_path.display()
                );
                log.line(&format!("==> 失败: {message}"));
                return Err(message);
            }
        };
        let label = format!("{} {}", identity.name, identity.version);
        let version = Version::parse(&identity.version)
            .map_err(|error| format!("配方 `{label}` 的版本非法: {error}"))?;
        let archive_path = repo_dir.join(format!("{}-{}.xpkg", identity.name, identity.version));

        log.line(&format!("==> [{}/{}] {label}", index + 1, recipes.len()));

        let archive = if archive_path.is_file() {
            report.skipped += 1;
            log.line(&format!(
                "    跳过构建: `{}` 已存在",
                archive_path.display()
            ));
            archive_path
        } else {
            let built = builder::build_package_with_options(
                recipe_path,
                repo_dir,
                BuildOptions {
                    sandbox: options.sandbox,
                    jobs: options.jobs,
                },
            );
            match built {
                Ok(path) => {
                    report.built += 1;
                    log.line(&format!("    构建完成: `{}`", path.display()));
                    path
                }
                Err(error) => {
                    let message =
                        format!("build-world 在 `{label}` 构建失败，后续包未处理: {error}");
                    log.line(&format!("    构建失败: {error}"));
                    log.line(&format!("==> 失败: {message}"));
                    return Err(message);
                }
            }
        };

        let metadata = match xpkg::read_metadata(&archive) {
            Ok(metadata) => metadata,
            Err(error) => {
                let message = format!(
                    "build-world 在 `{label}` 读取归档 `{}` 失败，后续包未处理: {error}",
                    archive.display()
                );
                log.line(&format!("==> 失败: {message}"));
                return Err(message);
            }
        };
        if metadata.name != identity.name || metadata.version != version {
            let message = format!(
                "归档 `{}` 的身份是 {} {}，与配方 `{label}` 不一致；后续包未处理",
                archive.display(),
                metadata.name,
                metadata.version
            );
            log.line(&format!("==> 失败: {message}"));
            return Err(message);
        }

        let installed = db.list_installed()?;
        if installed
            .iter()
            .any(|package| package.name == identity.name && package.version == version)
        {
            report.already_installed += 1;
            log.line("    已安装相同版本，跳过安装");
            continue;
        }

        let selected = SelectedPackage {
            name: metadata.name.clone(),
            version: metadata.version.clone(),
        };
        let mut archives = HashMap::new();
        archives.insert((selected.name.clone(), selected.version.clone()), archive);
        match installer::install_changes(
            &mut db,
            &[selected],
            &archives,
            rootfs_path,
            options.no_hooks,
            false,
        ) {
            Ok(()) => {
                report.installed += 1;
                log.line("    安装完成");
            }
            Err(error) => {
                let message = format!("build-world 在 `{label}` 安装失败，后续包未处理: {error}");
                log.line(&format!("    安装失败: {error}"));
                log.line(&format!("==> 失败: {message}"));
                return Err(message);
            }
        }
    }

    log.line(&format!(
        "==> build-world 完成: 构建 {}，跳过构建 {}，安装 {}，已安装 {}",
        report.built, report.skipped, report.installed, report.already_installed
    ));
    Ok(report)
}

/// 读取系统清单并把相对配方路径解析到清单所在目录。
fn load_recipes(config_path: &Path) -> Result<Vec<PathBuf>, String> {
    let text = fs::read_to_string(config_path)
        .map_err(|error| format!("读取系统清单 `{}` 失败: {error}", config_path.display()))?;
    let config: SystemConfig = toml::from_str(&text)
        .map_err(|error| format!("解析系统清单 `{}` 失败: {error}", config_path.display()))?;
    if config.recipes.is_empty() {
        return Err(format!(
            "系统清单 `{}` 的 recipes 不能为空",
            config_path.display()
        ));
    }

    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(config
        .recipes
        .into_iter()
        .map(|recipe| {
            let path = Path::new(&recipe);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_system_configuration_lists_recipes_in_dependency_order() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let recipes = load_recipes(&root.join("recipes/system.toml")).unwrap();
        let names = recipes
            .iter()
            .map(|path| builder::read_recipe_identity(path).unwrap().name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "glibc",
                "binutils",
                "gcc",
                "zlib",
                "gmp",
                "isl",
                "zstd",
                "ncurses",
                "bash",
                "coreutils",
                "linux"
            ]
        );
    }
}
