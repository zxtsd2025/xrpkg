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
use crate::installer;
use crate::resolvo_solver::incremental_plan;
use crate::sign;
use crate::solver::{Package, SelectedPackage};
use crate::state::LocalDb;
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
        println!("==> 更新仓库 {}: {}", repo.name, url);
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
        println!("    签名验证通过，包数量: {}", index.packages.len());
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
    };

    index
        .packages
        .retain(|package| package.name != entry.name || package.version != entry.version);
    index.packages.push(entry);
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
        let names_in_repo = repo
            .index
            .packages
            .iter()
            .map(|package| package.name.clone())
            .collect::<BTreeSet<_>>();

        for package in &repo.index.packages {
            if claimed_names.contains(&package.name) {
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
            println!("==> 下载包 {}: {}", package.name, url);
            fetch_package(&url, &target, &source.sha256, keyring)?;
        } else {
            println!("==> 包缓存命中且签名有效: {}", target.display());
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
            println!(
                "==> 清理缓存包: {} {} ({})",
                metadata.name,
                metadata.version,
                path.display()
            );
        }
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
/// 从缓存仓库安装指定包，先下载所有包，再进入事务性提交阶段。
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
    let repos = load_cached(config_path, cache_dir)?;
    let keyring = keyring_path(config_path)?;
    let pool = build_pool(&repos)?;
    let installed = db.list_installed()?;
    let resolution = crate::resolvo_solver::resolve_install_version(
        &pool.packages,
        requested,
        &installed,
        requested_version,
    )?;
    let changes = incremental_plan(&resolution, &installed);
    if changes.is_empty() {
        println!("{requested} 已经安装，无需操作");
        return Ok(());
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

    println!("安装计划:");
    for package in &changes {
        println!("{} {}", package.name, package.version);
    }
    let archives = download_packages(&pool, &changes, cache_dir, &keyring)?;
    installer::install_changes(
        db,
        &changes,
        &archives,
        rootfs_path,
        no_hooks,
        allow_downgrade,
    )
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
        if package.name.is_empty()
            || package
                .name
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
        {
            return Err(format!("仓库索引中的包名 `{}` 非法", package.name));
        }
        let version = Version::parse(&package.version)
            .map_err(|error| format!("仓库索引中的版本 `{}` 非法: {error}", package.version))?;
        let _ = version;
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
        if !identities.insert((package.name.clone(), package.version.clone())) {
            return Err(format!(
                "仓库索引中包 `{}` 版本 `{}` 重复",
                package.name, package.version
            ));
        }
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
            let left_version = Version::parse(&left.version).ok();
            let right_version = Version::parse(&right.version).ok();
            left_version.cmp(&right_version)
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
fn fetch_package(url: &str, target: &Path, expected: &str, keyring: &Path) -> Result<(), Error> {
    let partial = target.with_extension(format!("part-{}", unique_nonce()));
    let result = if is_http(url) {
        let response = ureq::get(url)
            .set("User-Agent", "xrpkg/0.1")
            .call()
            .map_err(|error| format!("下载 `{url}` 失败: {error}"))?;
        let mut reader = response.into_reader();
        let mut file =
            fs::File::create(&partial).map_err(|error| format!("创建下载临时文件失败: {error}"))?;
        std::io::copy(&mut reader, &mut file)
            .map_err(|error| format!("写入下载文件失败: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("刷新下载文件失败: {error}"))?;
        Ok(())
    } else {
        fs::copy(local_path(url)?, &partial)
            .map(|_| ())
            .map_err(|error| format!("复制包 `{url}` 失败: {error}"))
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    if let Err(error) = verify_sha256(&partial, expected) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }

    let signature_url = format!("{url}.sig");
    let pending_signature = sign::signature_path(&partial);
    let signature = match fetch_bytes(&signature_url) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(format!("包签名文件缺失或不可读: {error}"));
        }
    };
    if let Err(error) = fs::write(&pending_signature, signature) {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&pending_signature);
        return Err(format!("写入包签名临时文件失败: {error}"));
    }
    if let Err(error) = sign::verify_detached(keyring, &pending_signature, &partial) {
        let _ = fs::remove_file(&partial);
        let _ = fs::remove_file(&pending_signature);
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
    })
}

fn sha256_file(path: &Path) -> Result<String, Error> {
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
