//! 软件仓库：索引生成、索引缓存、包下载和仓库安装。
//!
//! 仓库索引是仓库根目录下的 `xrpkg-repo.toml`；客户端只缓存索引，
//! 安装时再按索引里的 filename + sha256 下载 `.xpkg`。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capability::{Capability, Version};
use crate::download;
use crate::installer;
use crate::mirrors::Mirror;
use crate::resolvo_solver::incremental_plan;
use crate::sign;
use crate::solver::{Package, SelectedPackage};
use crate::state::LocalDb;
use crate::ui;
use crate::xpkg::read_metadata;

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 仓库索引文件名。
const INDEX_FILENAME: &str = "xrpkg-repo.toml";

/// 一个仓库配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoConfig {
    pub name: String,
    pub url: String,
}

/// 已下载到本地缓存的仓库索引。
#[derive(Clone, Debug)]
pub struct CachedRepo {
    pub name: String,
    pub url: String,
    pub index: RepoIndex,
}

/// 仓库索引文件。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoIndex {
    #[serde(default = "default_format")]
    pub format: u32,
    pub name: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub packages: Vec<RepoPackage>,
}

/// 仓库中的一个包条目。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub requires: Vec<String>,
    pub filename: String,
    pub sha256: String,
    /// 外部生态标记；`None` 表示原生 `.xpkg`，`Some("arch")` 表示 Arch 包。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// 仓库包在哪个仓库、用什么 URL 下载。
#[derive(Clone, Debug)]
pub struct PackageSource {
    pub repo_name: String,
    pub repo_url: String,
    pub filename: String,
    pub sha256: String,
}

/// 按配置优先级构造出的候选池。
#[derive(Clone, Debug, Default)]
pub struct RepoPool {
    pub packages: Vec<Package>,
    sources: HashMap<(String, Version), PackageSource>,
}

impl RepoPool {
    /// 查找某个具体包版本的下载来源。
    pub fn source(&self, package: &SelectedPackage) -> Option<&PackageSource> {
        self.sources
            .get(&(package.name.clone(), package.version.clone()))
    }
}

/// 搜索结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub repo: String,
    pub name: String,
    pub version: String,
}

fn default_format() -> u32 {
    1
}

/// 默认全局配置路径：`~/.config/xrpkg/repos.toml`。
pub fn default_config_path() -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "无法确定 HOME，无法定位 ~/.config/xrpkg/repos.toml".to_string())?;
    Ok(PathBuf::from(home).join(".config/xrpkg/repos.toml"))
}

/// 读取全局仓库配置，按配置数组顺序返回。
pub fn load_config(path: &Path) -> Result<Vec<RepoConfig>, Error> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("读取仓库配置 `{}` 失败: {error}", path.display()))?;
    let document: ConfigDocument = toml::from_str(&text)
        .map_err(|error| format!("解析仓库配置 `{}` 失败: {error}", path.display()))?;

    if !document.repos.is_empty() && !document.repo.is_empty() {
        return Err("仓库配置不能同时使用 [[repos]] 和 [repo.*]".to_string());
    }

    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut repos = Vec::new();
    if !document.repos.is_empty() {
        for entry in document.repos {
            if entry.name.is_empty() {
                return Err("[[repos]] 的 name 不能为空".to_string());
            }
            repos.push(RepoConfig {
                name: entry.name,
                url: normalize_url(&entry.url, config_dir),
            });
        }
    } else {
        let mut legacy = document.repo.into_iter().collect::<Vec<_>>();
        legacy.sort_by(|left, right| {
            left.1
                .priority
                .cmp(&right.1.priority)
                .then_with(|| left.0.cmp(&right.0))
        });
        for (name, entry) in legacy {
            repos.push(RepoConfig {
                name,
                url: normalize_url(&entry.url, config_dir),
            });
        }
    }

    if repos.is_empty() {
        return Err(format!("仓库配置 `{}` 没有任何仓库", path.display()));
    }
    let mut names = BTreeSet::new();
    for repo in &repos {
        validate_repo_name(&repo.name)?;
        if !names.insert(repo.name.clone()) {
            return Err(format!("仓库配置中出现重复仓库名 `{}`", repo.name));
        }
    }
    Ok(repos)
}

/// 配置文件的 TOML 结构。
#[derive(Debug, Deserialize)]
struct ConfigDocument {
    #[serde(default)]
    keyring: Option<String>,
    #[serde(default)]
    repos: Vec<ConfigRepo>,
    #[serde(default)]
    repo: BTreeMap<String, ConfigRepo>,
}

/// 返回配置指定的信任公钥环；默认是配置文件同目录的 `trusted.gpg`。
pub fn keyring_path(config_path: &Path) -> Result<PathBuf, Error> {
    let text = fs::read_to_string(config_path)
        .map_err(|error| format!("读取仓库配置 `{}` 失败: {error}", config_path.display()))?;
    let document: ConfigDocument = toml::from_str(&text)
        .map_err(|error| format!("解析仓库配置 `{}` 失败: {error}", config_path.display()))?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let path = match document.keyring {
        Some(keyring) if !keyring.trim().is_empty() => {
            let path = Path::new(&keyring);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                config_dir.join(path)
            }
        }
        _ => config_dir.join("trusted.gpg"),
    };
    if !path.is_file() {
        return Err(format!("信任公钥环不存在: {}", path.display()));
    }
    Ok(path)
}

#[derive(Debug, Deserialize)]
struct ConfigRepo {
    #[serde(default)]
    name: String,
    url: String,
    #[serde(default)]
    priority: i64,
}

/// 把相对本地 URL 解析为相对于配置文件目录的绝对路径。
fn normalize_url(url: &str, config_dir: &Path) -> String {
    if is_http(url) || url.starts_with("file://") {
        url.to_string()
    } else {
        let path = Path::new(url);
        if path.is_absolute() {
            path.to_string_lossy().into_owned()
        } else {
            config_dir.join(path).to_string_lossy().into_owned()
        }
    }
}

/// 更新所有仓库索引到本地缓存。
pub fn update(config_path: &Path, cache_dir: &Path) -> Result<Vec<CachedRepo>, Error> {
    let repos = load_config(config_path)?;
    let keyring = keyring_path(config_path)?;
    let index_cache = cache_dir.join("indexes");
    fs::create_dir_all(&index_cache).map_err(|error| format!("创建索引缓存目录失败: {error}"))?;

    let mut cached = Vec::new();
    for repo in repos {
        let url = repo_index_url(&repo.url)?;
        let signature_url = format!("{url}.sig");
        ui::phase(&ui::pick(
            format!("更新仓库 {}: {}", repo.name, url),
            format!("Updating repository {}: {}", repo.name, url),
        ));
        let bytes = fetch_bytes(&url)?;
        let signature = fetch_bytes(&signature_url)
            .map_err(|error| format!("仓库索引签名文件缺失或不可读: {error}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|error| format!("仓库索引不是 UTF-8 文本: {error}"))?;
        let target = index_cache.join(format!("{}.toml", repo.name));
        let signature_target = sign::signature_path(&target);
        let pending = target.with_extension(format!("verify-{}", unique_nonce()));
        let pending_signature = sign::signature_path(&pending);
        fs::write(&pending, text.as_bytes())
            .map_err(|error| format!("写入待验证索引失败: {error}"))?;
        fs::write(&pending_signature, &signature)
            .map_err(|error| format!("写入待验证签名失败: {error}"))?;
        if let Err(error) = sign::verify_detached(&keyring, &pending_signature, &pending) {
            let _ = fs::remove_file(&pending);
            let _ = fs::remove_file(&pending_signature);
            return Err(error);
        }
        let index = match parse_index(&text, Some(&repo.name)) {
            Ok(index) => index,
            Err(error) => {
                let _ = fs::remove_file(&pending);
                let _ = fs::remove_file(&pending_signature);
                return Err(error);
            }
        };
        fs::rename(&pending, &target)
            .map_err(|error| format!("保存索引缓存 `{}` 失败: {error}", target.display()))?;
        fs::rename(&pending_signature, &signature_target).map_err(|error| {
            format!(
                "保存索引签名缓存 `{}` 失败: {error}",
                signature_target.display()
            )
        })?;
        ui::note(&ui::pick(
            format!("签名验证通过，包数量: {}", index.packages.len()),
            format!("Signature verified; {} packages", index.packages.len()),
        ));
        cached.push(CachedRepo {
            name: repo.name,
            url: repo.url,
            index,
        });
    }
    Ok(cached)
}

/// 读取本地缓存的全部仓库索引；不再访问网络。
pub fn load_cached(config_path: &Path, cache_dir: &Path) -> Result<Vec<CachedRepo>, Error> {
    let repos = load_config(config_path)?;
    let keyring = keyring_path(config_path)?;
    let index_cache = cache_dir.join("indexes");
    let mut cached = Vec::new();
    for repo in repos {
        let path = index_cache.join(format!("{}.toml", repo.name));
        if !path.is_file() {
            return Err(format!(
                "仓库 `{}` 尚未更新，请先运行 `xrpkg update`",
                repo.name
            ));
        }
        sign::verify_detached(&keyring, &sign::signature_path(&path), &path)?;
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("读取缓存索引 `{}` 失败: {error}", path.display()))?;
        let index = parse_index(&text, Some(&repo.name))?;
        cached.push(CachedRepo {
            name: repo.name,
            url: repo.url,
            index,
        });
    }
    Ok(cached)
}

/// 只搜索本地缓存索引。
pub fn search(
    config_path: &Path,
    cache_dir: &Path,
    keyword: &str,
) -> Result<Vec<SearchHit>, Error> {
    let keyword = keyword.to_lowercase();
    let repos = load_cached(config_path, cache_dir)?;
    let mut hits = Vec::new();
    for repo in repos {
        for package in &repo.index.packages {
            if package.name.to_lowercase().contains(&keyword) {
                hits.push(SearchHit {
                    repo: repo.name.clone(),
                    name: package.name.clone(),
                    version: package.version.clone(),
                });
            }
        }
    }
    Ok(hits)
}

/// 把 `.xpkg` 加入仓库目录，并更新 `xrpkg-repo.toml`；同时签名包和索引。
pub fn repo_add_signed(
    repo_dir: &Path,
    package_path: &Path,
    key_id: &str,
) -> Result<RepoIndex, Error> {
    let index = repo_add(repo_dir, package_path)?;
    let filename = package_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("无法取得包文件名 `{}`", package_path.display()))?;
    sign::sign_detached(key_id, &repo_dir.join(filename))?;
    sign::sign_detached(key_id, &repo_dir.join(INDEX_FILENAME))?;
    Ok(index)
}

/// 把 `.xpkg` 加入仓库目录，并更新 `xrpkg-repo.toml`。
pub fn repo_add(repo_dir: &Path, package_path: &Path) -> Result<RepoIndex, Error> {
    let metadata = read_metadata(package_path)?;
    let filename = package_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("无法取得包文件名 `{}`", package_path.display()))?
        .to_string();
    if filename.contains(['/', '\\']) || filename == "." || filename == ".." {
        return Err(format!("包文件名 `{filename}` 非法"));
    }

    fs::create_dir_all(repo_dir)
        .map_err(|error| format!("创建仓库目录 `{}` 失败: {error}", repo_dir.display()))?;
    let destination = repo_dir.join(&filename);
    let same_file = fs::canonicalize(package_path).ok() == fs::canonicalize(&destination).ok();
    if !same_file {
        fs::copy(package_path, &destination).map_err(|error| {
            format!(
                "复制包 `{}` -> `{}` 失败: {error}",
                package_path.display(),
                destination.display()
            )
        })?;
    }
    let sha256 = sha256_file(&destination)?;
    let entry = RepoPackage {
        name: metadata.name.clone(),
        version: metadata.version.to_string(),
        requires: metadata.requires.iter().map(ToString::to_string).collect(),
        filename,
        sha256,
        origin: None,
    };

    add_index_entries(repo_dir, &[entry])
}

/// 把一批已经落盘的包写入仓库索引；索引重建逻辑与 [`repo_add`] 共用。
///
/// 调用方负责先把文件放进 `repo_dir`；本函数只校验条目、去重并原子写回
/// `xrpkg-repo.toml`，不复制包文件。
pub fn add_index_entries(repo_dir: &Path, entries: &[RepoPackage]) -> Result<RepoIndex, Error> {
    fs::create_dir_all(repo_dir)
        .map_err(|error| format!("创建仓库目录 `{}` 失败: {error}", repo_dir.display()))?;
    let index_path = repo_dir.join(INDEX_FILENAME);
    let mut index = if index_path.is_file() {
        load_index(&index_path, None)?
    } else {
        RepoIndex {
            format: 1,
            name: repo_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("local")
                .to_string(),
            arch: std::env::consts::ARCH.to_string(),
            packages: Vec::new(),
        }
    };

    for entry in entries {
        validate_package_entry(entry)?;
        let package_path = repo_dir.join(&entry.filename);
        if !package_path.is_file() {
            return Err(format!(
                "仓库 `{}` 中缺少包文件 `{}`",
                repo_dir.display(),
                package_path.display()
            ));
        }
        // 原生包和外部包可以同名同版本共存，因此来源也是身份的一部分。
        index.packages.retain(|package| {
            package.name != entry.name
                || package.version != entry.version
                || package.origin != entry.origin
        });
        index.packages.push(entry.clone());
    }
    sort_packages(&mut index.packages);
    write_index(&index_path, &index)?;
    Ok(index)
}

/// 从仓库索引构造 resolvo 候选池，按配置顺序做包名级优先级覆盖。
pub fn build_pool(repos: &[CachedRepo]) -> Result<RepoPool, Error> {
    let mut packages = Vec::new();
    let mut sources = HashMap::new();
    let mut claimed_names = BTreeSet::new();

    for repo in repos {
        // 外部包暂时不参与原生 resolvo 求解，也不能抢走原生包的包名优先级。
        let names_in_repo = repo
            .index
            .packages
            .iter()
            .filter(|package| package.origin.is_none())
            .map(|package| package.name.clone())
            .collect::<BTreeSet<_>>();

        for package in &repo.index.packages {
            if package.origin.is_some() || claimed_names.contains(&package.name) {
                continue;
            }
            let version = Version::parse(&package.version).map_err(|error| {
                format!(
                    "仓库 `{}` 的包 `{}` 版本 `{}` 非法: {error}",
                    repo.name, package.name, package.version
                )
            })?;
            let requires = package
                .requires
                .iter()
                .map(|requirement| {
                    Capability::parse(requirement).map_err(|error| {
                        format!(
                            "仓库 `{}` 的包 `{}` 依赖 `{requirement}` 非法: {error}",
                            repo.name, package.name
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let key = (package.name.clone(), version.clone());
            if sources
                .insert(
                    key,
                    PackageSource {
                        repo_name: repo.name.clone(),
                        repo_url: repo.url.clone(),
                        filename: package.filename.clone(),
                        sha256: package.sha256.clone(),
                    },
                )
                .is_some()
            {
                return Err(format!(
                    "仓库 `{}` 中包 `{}` 版本 `{}` 重复",
                    repo.name, package.name, package.version
                ));
            }
            packages.push(Package::new(package.name.clone(), version, requires));
        }
        claimed_names.extend(names_in_repo);
    }

    Ok(RepoPool { packages, sources })
}

/// 下载安装计划中的所有 `.xpkg` 到缓存；任何一个失败都不会进入提交阶段。
pub fn download_packages(
    pool: &RepoPool,
    changes: &[SelectedPackage],
    cache_dir: &Path,
    keyring: &Path,
) -> Result<HashMap<(String, Version), PathBuf>, Error> {
    let mut archives = HashMap::new();
    for package in changes {
        let source = pool
            .source(package)
            .ok_or_else(|| format!("包 {} {} 没有仓库来源", package.name, package.version))?;
        let target_dir = cache_dir.join("packages").join(&source.repo_name);
        fs::create_dir_all(&target_dir).map_err(|error| format!("创建包缓存目录失败: {error}"))?;
        let target = target_dir.join(&source.filename);
        let signature = sign::signature_path(&target);

        let cached = target.is_file()
            && signature.is_file()
            && verify_sha256(&target, &source.sha256).is_ok()
            && sign::verify_detached(keyring, &signature, &target).is_ok();
        if !cached {
            let url = join_repo_url(&source.repo_url, &source.filename)?;
            fetch_package(&url, &target, &source.sha256, keyring, &package.name)?;
        } else {
            ui::step(&ui::pick(
                format!("包缓存命中且签名有效: {}", target.display()),
                format!(
                    "Package cache hit and signature valid: {}",
                    target.display()
                ),
            ));
        }
        archives.insert((package.name.clone(), package.version.clone()), target);
    }
    Ok(archives)
}

/// 缓存清理结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanReport {
    pub packages: usize,
    pub signatures: usize,
    pub bytes: u64,
}

/// 清理不再需要的 `.xpkg` 缓存。
///
/// 保留两类包：数据库中当前安装的版本，以及当前仓库索引中的最新版本。
pub fn clean_cache(
    config_path: &Path,
    cache_dir: &Path,
    db: &LocalDb,
) -> Result<CleanReport, Error> {
    let repos = load_cached(config_path, cache_dir)?;
    let installed = db.list_installed()?;
    let installed_versions = installed
        .iter()
        .map(|package| (package.name.clone(), package.version.clone()))
        .collect::<HashMap<_, _>>();
    let mut latest_versions = HashMap::<String, Version>::new();
    for repo in &repos {
        for package in &repo.index.packages {
            if package.origin.is_some() {
                continue;
            }
            let version = Version::parse(&package.version).map_err(|error| {
                format!(
                    "仓库 `{}` 的包 `{}` 版本 `{}` 非法: {error}",
                    repo.name, package.name, package.version
                )
            })?;
            latest_versions
                .entry(package.name.clone())
                .and_modify(|current| {
                    if version > *current {
                        *current = version.clone();
                    }
                })
                .or_insert(version);
        }
    }

    let packages_root = cache_dir.join("packages");
    if !packages_root.is_dir() {
        return Ok(CleanReport::default());
    }
    let mut report = CleanReport::default();
    for repo_entry in fs::read_dir(&packages_root)
        .map_err(|error| format!("读取包缓存目录 `{}` 失败: {error}", packages_root.display()))?
    {
        let repo_entry = repo_entry.map_err(|error| format!("读取包缓存条目失败: {error}"))?;
        if !repo_entry
            .file_type()
            .map_err(|error| format!("读取缓存条目类型失败: {error}"))?
            .is_dir()
        {
            continue;
        }
        let directory = repo_entry.path();
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("读取缓存目录 `{}` 失败: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("读取缓存文件失败: {error}"))?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("xpkg") {
                continue;
            }
            let metadata = read_metadata(&path)?;
            let keep_installed = installed_versions
                .get(&metadata.name)
                .is_some_and(|version| *version == metadata.version);
            let keep_latest = latest_versions
                .get(&metadata.name)
                .is_some_and(|version| *version == metadata.version);
            if keep_installed || keep_latest {
                continue;
            }
            let signature = sign::signature_path(&path);
            let size = fs::metadata(&path)
                .map_err(|error| format!("读取缓存文件大小失败: {error}"))?
                .len();
            fs::remove_file(&path)
                .map_err(|error| format!("删除缓存包 `{}` 失败: {error}", path.display()))?;
            report.packages += 1;
            report.bytes += size;
            if signature.is_file() {
                let signature_size = fs::metadata(&signature)
                    .map_err(|error| format!("读取签名文件大小失败: {error}"))?
                    .len();
                fs::remove_file(&signature).map_err(|error| {
                    format!("删除缓存签名 `{}` 失败: {error}", signature.display())
                })?;
                report.signatures += 1;
                report.bytes += signature_size;
            }
            ui::step(&ui::pick(
                format!(
                    "清理缓存包: {} {} ({})",
                    metadata.name,
                    metadata.version,
                    path.display()
                ),
                format!(
                    "Removing cached package: {} {} ({})",
                    metadata.name,
                    metadata.version,
                    path.display()
                ),
            ));
        }
    }
    Ok(report)
}

/// 安装请求最终落到哪条轨道。
#[derive(Clone, Debug)]
pub enum RepositoryInstall {
    /// 原生 `.xpkg` 已在 `repo::install_from_repositories_with_mirrors` 内完成提交。
    Native,
    /// 需要调用方把下载好的 Arch 包交给兼容轨道安装器。
    Arch(ArchDownload),
}

/// 已下载到本地缓存的 Arch 包。
#[derive(Clone, Debug)]
pub struct ArchDownload {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
/// 从缓存仓库安装指定包；保留原有签名，仅处理原生 `.xpkg`。
pub fn install_from_repositories(
    db: &mut LocalDb,
    config_path: &Path,
    cache_dir: &Path,
    requested: &str,
    requested_version: Option<&Version>,
    allow_downgrade: bool,
    rootfs_path: &Path,
    no_hooks: bool,
) -> Result<(), Error> {
    install_from_repositories_with_mirrors(
        db,
        config_path,
        cache_dir,
        requested,
        requested_version,
        allow_downgrade,
        rootfs_path,
        no_hooks,
        None,
        false,
    )
    .map(|_| ())
}

/// 从缓存仓库安装指定包，并在原生 `.xpkg` 与 Arch `.pkg.tar.zst` 之间分派。
///
/// 默认原生优先；只有 `--prefer-arch` 或者没有原生候选时才走 Arch 轨道。
/// Arch 包只下载到缓存并返回，真正的安装由 CLI 调用现有兼容轨道逻辑完成。
#[allow(clippy::too_many_arguments)]
pub fn install_from_repositories_with_mirrors(
    db: &mut LocalDb,
    config_path: &Path,
    cache_dir: &Path,
    requested: &str,
    requested_version: Option<&Version>,
    allow_downgrade: bool,
    rootfs_path: &Path,
    no_hooks: bool,
    mirrors_path: Option<&Path>,
    prefer_arch: bool,
) -> Result<RepositoryInstall, Error> {
    let repos = load_cached(config_path, cache_dir)?;
    let keyring = keyring_path(config_path)?;
    let pool = build_pool(&repos)?;
    let installed = db.list_installed()?;
    let arch_package = select_arch_package(&repos, requested);
    let native_available = pool
        .packages
        .iter()
        .any(|package| package.name == requested);

    if prefer_arch_package(prefer_arch, arch_package.is_some(), native_available) {
        let package = arch_package.ok_or_else(|| format!("没有找到 Arch 兼容包 `{requested}`"))?;
        if requested_version.is_some() {
            return Err(format!(
                "Arch 兼容包 `{requested}` 暂不支持 `包=版本` 语法，请直接使用包名"
            ));
        }
        let mirrors_path = mirrors_path.ok_or_else(|| {
            "安装 Arch 兼容包需要镜像配置；请提供 --mirrors 或创建 config/mirrors.toml".to_string()
        })?;
        let mirrors = crate::mirrors::load(mirrors_path)?;
        let path = download_arch_package(package, cache_dir, &mirrors)?;
        return Ok(RepositoryInstall::Arch(ArchDownload {
            name: package.name.clone(),
            version: package.version.clone(),
            path,
        }));
    }

    let resolution = crate::resolvo_solver::resolve_install_version(
        &pool.packages,
        requested,
        &installed,
        requested_version,
    )?;
    let changes = incremental_plan(&resolution, &installed);
    if changes.is_empty() {
        ui::note(&ui::pick(
            format!("{requested} 已经安装，无需操作"),
            format!("{requested} is already installed"),
        ));
        return Ok(RepositoryInstall::Native);
    }
    if !allow_downgrade {
        for change in &changes {
            if let Some(old) = installed.iter().find(|package| package.name == change.name) {
                if change.version < old.version {
                    return Err(format!(
                        "拒绝降级: {} {} -> {}；如确需降级请加 --allow-downgrade",
                        old.name, old.version_raw, change.version
                    ));
                }
            }
        }
    }

    ui::phase(ui::t("Install plan", "安装计划"));
    let rows = changes
        .iter()
        .map(|package| [package.name.clone(), package.version.to_string()])
        .collect::<Vec<_>>();
    ui::table2([ui::t("Package", "包"), ui::t("Version", "版本")], &rows);
    let archives = download_packages(&pool, &changes, cache_dir, &keyring)?;
    installer::install_changes(
        db,
        &changes,
        &archives,
        rootfs_path,
        no_hooks,
        allow_downgrade,
    )?;
    Ok(RepositoryInstall::Native)
}

/// 原生优先策略：只有显式 `--prefer-arch` 或没有原生候选时才选 Arch。
fn prefer_arch_package(prefer_arch: bool, has_arch: bool, has_native: bool) -> bool {
    has_arch && (prefer_arch || !has_native)
}

/// 在缓存索引中查找指定名称的 Arch 包。
fn select_arch_package<'a>(repos: &'a [CachedRepo], requested: &str) -> Option<&'a RepoPackage> {
    for repo in repos {
        for package in &repo.index.packages {
            if package.name == requested && package.origin.as_deref() == Some("arch") {
                return Some(package);
            }
        }
    }
    None
}

/// 按镜像优先级下载 Arch 包到缓存；失败时自动尝试下一个镜像。
fn download_arch_package(
    package: &RepoPackage,
    cache_dir: &Path,
    mirrors: &[Mirror],
) -> Result<PathBuf, Error> {
    let target_dir = cache_dir.join("packages").join("arch");
    fs::create_dir_all(&target_dir).map_err(|error| {
        format!(
            "创建 Arch 包缓存目录 `{}` 失败: {error}",
            target_dir.display()
        )
    })?;
    let target = target_dir.join(&package.filename);
    if target.is_file() && verify_sha256(&target, &package.sha256).is_ok() {
        ui::step(&ui::pick(
            format!("Arch 包缓存命中: {}", target.display()),
            format!("Arch package cache hit: {}", target.display()),
        ));
        return Ok(target);
    }

    let mut failures = Vec::new();
    for mirror in mirrors {
        let url = download::join_location(&mirror.url, &package.filename)?;
        ui::phase(&ui::pick(
            format!(
                "从镜像 {} 下载 Arch 包 {} {}: {}",
                mirror.name, package.name, package.version, url
            ),
            format!(
                "Downloading Arch package {} {} from mirror {}: {}",
                package.name, package.version, mirror.name, url
            ),
        ));
        let bar = download::single_bar(
            0,
            &ui::pick(
                format!("下载 {}", package.name),
                format!("Downloading {}", package.name),
            ),
        );
        let partial = target.with_extension(format!("part-{}", unique_nonce()));
        let downloaded = download::to_file(&url, &partial, &bar).inspect_err(|_| {
            let _ = fs::remove_file(&partial);
            bar.abandon_with_message(ui::pick(
                format!("失败 {}", package.name),
                format!("Failed {}", package.name),
            ));
        });
        let downloaded = match downloaded {
            Ok(written) => written,
            Err(error) => {
                failures.push(format!("{}: {error}", mirror.name));
                continue;
            }
        };
        if let Err(error) = verify_sha256(&partial, &package.sha256) {
            let _ = fs::remove_file(&partial);
            bar.abandon_with_message(format!("校验失败 {}", package.name));
            failures.push(format!("{}: {error}", mirror.name));
            continue;
        }
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        fs::rename(&partial, &target).map_err(|error| {
            let _ = fs::remove_file(&partial);
            format!("保存 Arch 包 `{}` 失败: {error}", target.display())
        })?;
        bar.finish_with_message(format!("完成 {} ({} bytes)", package.name, downloaded));
        return Ok(target);
    }

    Err(format!("所有镜像都下载失败: {}", failures.join("; ")))
}

/// 把一行仓库索引文本解析并校验。
fn parse_index(text: &str, expected_name: Option<&str>) -> Result<RepoIndex, Error> {
    let index: RepoIndex =
        toml::from_str(text).map_err(|error| format!("解析仓库索引失败: {error}"))?;
    validate_index(&index, expected_name)?;
    Ok(index)
}

/// 加载仓库目录中的索引。
fn load_index(path: &Path, expected_name: Option<&str>) -> Result<RepoIndex, Error> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("读取仓库索引 `{}` 失败: {error}", path.display()))?;
    parse_index(&text, expected_name)
}

/// 校验索引格式和所有包条目。
fn validate_index(index: &RepoIndex, expected_name: Option<&str>) -> Result<(), Error> {
    if index.format != 1 {
        return Err(format!("不支持的仓库索引格式版本 {}", index.format));
    }
    validate_repo_name(&index.name)?;
    if let Some(expected) = expected_name {
        if index.name != expected {
            return Err(format!(
                "仓库索引名称 `{}` 与配置中的 `{expected}` 不一致",
                index.name
            ));
        }
    }
    if index.arch.is_empty() {
        return Err("仓库索引缺少 arch".to_string());
    }

    let mut identities = BTreeSet::new();
    for package in &index.packages {
        validate_package_entry(package)?;
        if !identities.insert((
            package.name.clone(),
            package.version.clone(),
            package.origin.clone(),
        )) {
            return Err(format!(
                "仓库索引中包 `{}` 版本 `{}` 重复",
                package.name, package.version
            ));
        }
    }
    Ok(())
}

/// 校验单个索引条目；原生包要求 native Version，外部包保留原始版本字符串。
fn validate_package_entry(package: &RepoPackage) -> Result<(), Error> {
    if package.name.is_empty()
        || package
            .name
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
    {
        return Err(format!("仓库索引中的包名 `{}` 非法", package.name));
    }
    match &package.origin {
        Some(origin) => {
            if origin.is_empty()
                || origin
                    .chars()
                    .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
            {
                return Err(format!("仓库索引中的来源标记 `{origin}` 非法"));
            }
            if package.version.is_empty() || package.version.chars().any(char::is_whitespace) {
                return Err(format!(
                    "仓库索引中外部包 `{}` 的版本 `{}` 非法",
                    package.name, package.version
                ));
            }
        }
        None => {
            Version::parse(&package.version)
                .map_err(|error| format!("仓库索引中的版本 `{}` 非法: {error}", package.version))?;
        }
    }
    for requirement in &package.requires {
        Capability::parse(requirement)
            .map_err(|error| format!("仓库索引中的依赖 `{requirement}` 非法: {error}"))?;
    }
    if package.filename.is_empty()
        || package.filename.contains(['/', '\\'])
        || package.filename == "."
        || package.filename == ".."
    {
        return Err(format!("仓库索引中的文件名 `{}` 非法", package.filename));
    }
    if package.sha256.len() != 64
        || !package
            .sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!("仓库索引中的包 `{}` 的 sha256 非法", package.name));
    }
    Ok(())
}

fn validate_repo_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
    {
        return Err(format!("仓库名 `{name}` 非法"));
    }
    Ok(())
}

fn sort_packages(packages: &mut [RepoPackage]) {
    packages.sort_by(|left, right| {
        left.name.cmp(&right.name).then_with(|| {
            match (
                Version::parse(&left.version),
                Version::parse(&right.version),
            ) {
                (Ok(left_version), Ok(right_version)) => left_version.cmp(&right_version),
                _ => left.version.cmp(&right.version),
            }
        })
    });
}

fn write_index(path: &Path, index: &RepoIndex) -> Result<(), Error> {
    let text =
        toml::to_string_pretty(index).map_err(|error| format!("序列化仓库索引失败: {error}"))?;
    write_atomic(path, text.as_bytes())
}

fn repo_index_url(base: &str) -> Result<String, Error> {
    if is_http(base) {
        Ok(format!("{}/{}", base.trim_end_matches('/'), INDEX_FILENAME))
    } else {
        let path = local_path(base)?;
        Ok(path.join(INDEX_FILENAME).to_string_lossy().into_owned())
    }
}

fn join_repo_url(base: &str, filename: &str) -> Result<String, Error> {
    if is_http(base) {
        Ok(format!("{}/{}", base.trim_end_matches('/'), filename))
    } else {
        let path = local_path(base)?;
        Ok(path.join(filename).to_string_lossy().into_owned())
    }
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn local_path(url: &str) -> Result<PathBuf, Error> {
    if let Some(rest) = url.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        Ok(PathBuf::from(rest))
    } else {
        Ok(PathBuf::from(url))
    }
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>, Error> {
    if is_http(url) {
        let response = ureq::get(url)
            .set("User-Agent", "xrpkg/0.1")
            .call()
            .map_err(|error| format!("下载 `{url}` 失败: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 `{url}` 失败: {error}"))?;
        Ok(bytes)
    } else {
        fs::read(local_path(url)?).map_err(|error| format!("读取 `{url}` 失败: {error}"))
    }
}

/// 下载单个包到缓存，校验 SHA256 和发布者 detached signature。
fn fetch_package(
    url: &str,
    target: &Path,
    expected: &str,
    keyring: &Path,
    label: &str,
) -> Result<(), Error> {
    let partial = target.with_extension(format!("part-{}", unique_nonce()));
    if !download::progress_enabled() {
        ui::phase(&ui::pick(
            format!("下载包 {label}: {url}"),
            format!("Downloading package {label}: {url}"),
        ));
    }
    let bar = download::single_bar(
        0,
        &ui::pick(format!("下载 {label}"), format!("Downloading {label}")),
    );
    if let Err(error) = download::to_file(url, &partial, &bar) {
        let _ = fs::remove_file(&partial);
        bar.abandon_with_message(ui::pick(format!("失败 {label}"), format!("Failed {label}")));
        return Err(error);
    }
    if let Err(error) = verify_sha256(&partial, expected) {
        let _ = fs::remove_file(&partial);
        bar.abandon_with_message(ui::pick(
            format!("校验失败 {label}"),
            format!("Checksum failed {label}"),
        ));
        return Err(error);
    }

    let signature_url = format!("{url}.sig");
    let pending_signature = sign::signature_path(&partial);
    let signature = match fetch_bytes(&signature_url) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            bar.abandon_with_message(format!("缺少签名 {label}"));
            return Err(format!("包签名文件缺失或不可读: {error}"));
        }
    };
    if let Err(error) = fs::write(&pending_signature, signature) {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&pending_signature);
        bar.abandon_with_message(format!("签名写入失败 {label}"));
        return Err(format!("写入包签名临时文件失败: {error}"));
    }
    if let Err(error) = sign::verify_detached(keyring, &pending_signature, &partial) {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&pending_signature);
        bar.abandon_with_message(format!("签名无效 {label}"));
        return Err(error);
    }
    fs::rename(&partial, target).map_err(|error| {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&pending_signature);
        format!("保存包缓存 `{}` 失败: {error}", target.display())
    })?;
    fs::rename(&pending_signature, sign::signature_path(target)).map_err(|error| {
        let _ = fs::remove_file(&pending_signature);
        format!("保存包签名缓存失败: {error}")
    })?;
    bar.finish_with_message(format!("完成 {label}"));
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, Error> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("打开文件 `{}` 失败: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("读取文件 `{}` 失败: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), Error> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "包 `{}` 的 SHA256 不匹配: 期望 {expected}，实际 {actual}",
            path.display()
        ))
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let partial = path.with_extension(format!("tmp-{}", unique_nonce()));
    fs::write(&partial, bytes)
        .map_err(|error| format!("写入临时文件 `{}` 失败: {error}", partial.display()))?;
    fs::rename(&partial, path).map_err(|error| {
        let _ = fs::remove_file(&partial);
        format!("替换文件 `{}` 失败: {error}", path.display())
    })
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
        ^ u128::from(std::process::id())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn arch_repo() -> CachedRepo {
        CachedRepo {
            name: "repo".to_string(),
            url: "file:///srv/repo".to_string(),
            index: RepoIndex {
                format: 1,
                name: "repo".to_string(),
                arch: "x86_64".to_string(),
                packages: vec![RepoPackage {
                    name: "acl".to_string(),
                    version: "2.4.0-1".to_string(),
                    requires: vec!["glibc".to_string()],
                    filename: "acl-2.4.0-1-x86_64.pkg.tar.zst".to_string(),
                    sha256: "0".repeat(64),
                    origin: Some("arch".to_string()),
                }],
            },
        }
    }

    #[test]
    fn native_package_wins_unless_prefer_arch_is_set() {
        assert!(!prefer_arch_package(false, true, true));
        assert!(prefer_arch_package(true, true, true));
        assert!(!prefer_arch_package(true, false, true));
        assert!(prefer_arch_package(false, true, false));
    }

    #[test]
    fn selects_arch_entries_by_name() {
        let repos = [arch_repo()];

        assert!(select_arch_package(&repos, "acl").is_some());
        assert!(select_arch_package(&repos, "missing").is_none());
    }
}
