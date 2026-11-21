//! 跨生态仓库同步器：把 Pacman/Arch 仓库转换成 xrpkg 的本地仓库。
//!
//! 流程分为四步，任何一步失败都不会污染已有的仓库索引：
//! 1. 读取 `<mirror>/core.db`（tar.gz），解析每个包的 `desc`；
//! 2. 用 `config/deps_map.toml` 把外部依赖映射为内部 Capability，并与
//!    XROS 的 GLIBC 基线比较；
//! 3. 多线程下载 `.pkg.tar.zst`，用颜色进度条展示包名、速度、百分比；
//!    每个包都要通过 SHA256 和 ABI 守门员检查；
//! 4. 只有通过检查的包才写入 `repo/`，最后一次性重建 `xrpkg-repo.toml`。

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use indicatif::ProgressBar;

use crate::abi::{AbiGuard, MAX_GLIBC_VERSION};
use crate::capability::{Capability, Version};
use crate::compat_arch::verify_arch_abi;
use crate::deps_map::DependencyMapper;
use crate::download;
use crate::repo::{self, RepoPackage};
use crate::ui;

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// Arch 仓库数据库的固定文件名。
const DB_FILENAME: &str = "core.db";

/// 单个 `desc` 的大小上限，避免异常数据库耗尽内存。
const MAX_DESC_BYTES: u64 = 1024 * 1024;

/// 外部包在仓库索引中的来源标记。
const ARCH_ORIGIN: &str = "arch";

/// 一次同步的并发参数。
#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// 同时下载的包数量；会被实际包数量截断。
    pub jobs: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            jobs: default_jobs(),
        }
    }
}

/// 默认下载并发度：CPU 核心数，限制在 1..=8，避免把镜像打爆。
pub fn default_jobs() -> usize {
    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

/// 从 Arch 数据库 `desc` 中提取出的最小同步信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchRepoEntry {
    pub name: String,
    pub version: String,
    pub depends: Vec<String>,
    pub filename: String,
    pub sha256: Option<String>,
    pub csize: Option<u64>,
}

/// 一次同步的统计结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub synced: usize,
    pub skipped_dependencies: usize,
    pub skipped_abi: usize,
    pub failed: usize,
}

/// XROS 当前能够提供的基线 Capability。
///
/// 目前唯一硬约束是 GLIBC 符号版本；未在基线中的依赖名视为“由其它包提供”，
/// 不做拦截，避免把整个仓库的普通依赖误判成不可满足。
#[derive(Clone, Debug)]
pub struct SystemBaseline {
    capabilities: BTreeMap<String, Version>,
}

impl SystemBaseline {
    /// 构造 XROS 的默认基线，GLIBC 上限来自 ABI 守门员。
    pub fn xros() -> Result<Self, Error> {
        let glibc = Version::parse(MAX_GLIBC_VERSION)
            .map_err(|error| format!("ABI 基线版本无效: {error}"))?;
        let mut capabilities = BTreeMap::new();
        capabilities.insert("glibc".to_string(), glibc);
        Ok(Self { capabilities })
    }

    /// 查询基线中某个 Capability 的可用版本。
    pub fn version_of(&self, name: &str) -> Option<&Version> {
        self.capabilities.get(name)
    }
}

/// 同步一个 Arch 镜像目录，使用默认并发度。
///
/// `mirror_url` 可以是 `https://.../core/os/x86_64/`、`file:///path/mirror`
/// 或普通本地目录；`repo_dir` 是本地仓库目录，`deps_map_path` 是依赖映射表。
pub fn sync_mirror(
    mirror_url: &str,
    repo_dir: &Path,
    deps_map_path: &Path,
) -> Result<SyncReport, Error> {
    sync_mirror_with_options(mirror_url, repo_dir, deps_map_path, &SyncOptions::default())
}

/// 带并发参数和进度条的同步入口。
pub fn sync_mirror_with_options(
    mirror_url: &str,
    repo_dir: &Path,
    deps_map_path: &Path,
    options: &SyncOptions,
) -> Result<SyncReport, Error> {
    let mapper = DependencyMapper::load_or_create(deps_map_path)?;
    let baseline = SystemBaseline::xros()?;
    let guard = AbiGuard::new(MAX_GLIBC_VERSION)?;
    let entries = load_arch_entries(mirror_url)?;
    ui::phase(&ui::pick(
        format!("Arch 数据库条目: {}", entries.len()),
        format!("Arch database entries: {}", entries.len()),
    ));

    fs::create_dir_all(repo_dir)
        .map_err(|error| format!("创建本地仓库目录 `{}` 失败: {error}", repo_dir.display()))?;

    let mut report = SyncReport::default();
    let mut candidates = Vec::new();
    for entry in entries {
        let dependencies = match map_dependencies(&entry.depends, &mapper) {
            Ok(dependencies) => dependencies,
            Err(error) => {
                ui::warn(&ui::pick(
                    format!("跳过包 {} {}：{error}", entry.name, entry.version),
                    format!("Skipping package {} {}: {error}", entry.name, entry.version),
                ));
                report.failed += 1;
                continue;
            }
        };

        if let Some((source, mapped)) = first_unsatisfied(&dependencies, &baseline) {
            ui::warn(&ui::pick(
                format!(
                    "跳过包 {} {}：依赖 `{source}` 映射为 `{mapped}`，无法由 XROS 基线满足",
                    entry.name, entry.version
                ),
                format!(
                    "Skipping package {} {}: dependency `{source}` maps to `{mapped}`, not satisfied by the XROS baseline",
                    entry.name, entry.version
                ),
            ));
            report.skipped_dependencies += 1;
            continue;
        }

        let requires = dependencies
            .into_iter()
            .map(|(_, capability)| capability.to_string())
            .collect();
        candidates.push((entry, requires));
    }

    let total = candidates.len();
    if total == 0 {
        ui::ok(&ui::pick(
            format!(
                "同步完成: 收录 0，依赖跳过 {}，ABI 拦截 0，失败 {}",
                report.skipped_dependencies, report.failed
            ),
            format!(
                "Sync complete: 0 added, {} dependency skips, 0 ABI rejects, {} failed",
                report.skipped_dependencies, report.failed
            ),
        ));
        return Ok(report);
    }

    let workers = options.jobs.max(1).min(total);
    ui::phase(&ui::pick(
        format!("并发下载 {total} 个包（{workers} 个线程）"),
        format!("Downloading {total} package(s) with {workers} worker(s)"),
    ));
    let results = download_candidates(mirror_url, repo_dir, &candidates, &guard, workers);

    let mut indexed = Vec::with_capacity(total);
    for (position, (entry, requires)) in candidates.into_iter().enumerate() {
        match &results[position] {
            Some(Ok(success)) => {
                if success.missing_sha {
                    ui::warn(&ui::pick(
                        format!(
                            "包 {} 的 desc 缺少 SHA256SUM，记录下载后实测哈希",
                            entry.filename
                        ),
                        format!(
                            "Package {} has no SHA256SUM in desc; recording measured hash",
                            entry.filename
                        ),
                    ));
                }
                ui::step(&ui::pick(
                    format!("收录 {} {} ({})", entry.name, entry.version, entry.filename),
                    format!(
                        "Indexed {} {} ({})",
                        entry.name, entry.version, entry.filename
                    ),
                ));
                indexed.push(RepoPackage {
                    name: entry.name,
                    version: entry.version,
                    requires,
                    filename: entry.filename,
                    sha256: success.sha256.clone(),
                    origin: Some(ARCH_ORIGIN.to_string()),
                });
                report.synced += 1;
            }
            Some(Err(IngestError::Abi(error))) => {
                ui::warn(&ui::pick(
                    format!("ABI 拦截包 {} {}：{error}", entry.name, entry.version),
                    format!(
                        "ABI rejected package {} {}: {error}",
                        entry.name, entry.version
                    ),
                ));
                report.skipped_abi += 1;
            }
            Some(Err(IngestError::Other(error))) => {
                ui::warn(&ui::pick(
                    format!("跳过包 {} {}：{error}", entry.name, entry.version),
                    format!("Skipping package {} {}: {error}", entry.name, entry.version),
                ));
                report.failed += 1;
            }
            None => {
                ui::warn(&ui::pick(
                    format!("包 {} {} 没有返回下载结果", entry.name, entry.version),
                    format!(
                        "Package {} {} produced no download result",
                        entry.name, entry.version
                    ),
                ));
                report.failed += 1;
            }
        }
    }

    if !indexed.is_empty() {
        let index = repo::add_index_entries(repo_dir, &indexed)?;
        ui::ok(&ui::pick(
            format!(
                "仓库索引已更新: {} ({} 个包)",
                repo_dir.join("xrpkg-repo.toml").display(),
                index.packages.len()
            ),
            format!(
                "Repository index updated: {} ({} packages)",
                repo_dir.join("xrpkg-repo.toml").display(),
                index.packages.len()
            ),
        ));
    }

    ui::ok(&ui::pick(
        format!(
            "同步完成: 收录 {}，依赖跳过 {}，ABI 拦截 {}，失败 {}",
            report.synced, report.skipped_dependencies, report.skipped_abi, report.failed
        ),
        format!(
            "Sync complete: {} added, {} dependency skips, {} ABI rejects, {} failed",
            report.synced, report.skipped_dependencies, report.skipped_abi, report.failed
        ),
    ));
    Ok(report)
}

/// 用固定大小的线程池下载候选包；返回按候选顺序排列的结果。
fn download_candidates(
    mirror_url: &str,
    repo_dir: &Path,
    candidates: &[(ArchRepoEntry, Vec<String>)],
    guard: &AbiGuard,
    workers: usize,
) -> Vec<Option<Result<IngestSuccess, IngestError>>> {
    let total = candidates.len();
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
    let multi = download::multi_progress();

    thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let next = &next;
            let multi = &multi;
            scope.spawn(move || loop {
                let position = next.fetch_add(1, Ordering::Relaxed);
                if position >= total {
                    break;
                }
                let (entry, _) = &candidates[position];
                let bar = multi.add(download::multi_bar(entry.csize.unwrap_or(0), &entry.name));
                let result = ingest_package(mirror_url, repo_dir, entry, guard, &bar);
                bar.finish_and_clear();
                if sender.send((position, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
    });

    let mut results = (0..total).map(|_| None).collect::<Vec<_>>();
    for (position, result) in receiver {
        results[position] = Some(result);
    }
    results
}

/// 下载并解析 `<mirror>/core.db`。
fn load_arch_entries(mirror_url: &str) -> Result<Vec<ArchRepoEntry>, Error> {
    let url = download::join_location(mirror_url, DB_FILENAME)?;
    ui::phase(&ui::pick(
        format!("读取 Arch 数据库: {url}"),
        format!("Reading Arch database: {url}"),
    ));
    let bytes = download::read_all(&url)?;
    parse_core_db(&bytes)
}

/// 解压 tar.gz 数据库，逐个解析 `desc` 文本文件。
fn parse_core_db(bytes: &[u8]) -> Result<Vec<ArchRepoEntry>, Error> {
    let decoder = GzDecoder::new(io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut entries = Vec::new();

    for entry in archive
        .entries()
        .map_err(|error| format!("读取 Arch 数据库 TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 Arch 数据库条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 Arch 数据库条目路径失败: {error}"))?
            .into_owned();
        if path.file_name().and_then(|name| name.to_str()) != Some("desc")
            || !entry.header().entry_type().is_file()
        {
            continue;
        }
        if entry.size() > MAX_DESC_BYTES {
            return Err(format!(
                "Arch 数据库条目 `{}` 的 desc 超过 {} 字节上限",
                path.display(),
                MAX_DESC_BYTES
            ));
        }

        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| format!("读取 Arch 数据库条目 `{}` 失败: {error}", path.display()))?;
        entries.push(parse_desc(&text, &path)?);
    }

    if entries.is_empty() {
        return Err("Arch 数据库中没有任何 desc 条目".to_string());
    }
    Ok(entries)
}

/// 解析单个 `desc` 文件；`source` 只用于错误信息。
fn parse_desc(text: &str, source: &Path) -> Result<ArchRepoEntry, Error> {
    let sections = parse_sections(text);
    let name = single_field(&sections, "NAME", source)?;
    let version = single_field(&sections, "VERSION", source)?;
    let arch = optional_field(&sections, "ARCH").unwrap_or_else(|| "x86_64".to_string());
    let filename = optional_field(&sections, "FILENAME")
        .unwrap_or_else(|| format!("{name}-{version}-{arch}.pkg.tar.zst"));
    let sha256 = optional_field(&sections, "SHA256SUM");
    let csize = optional_field(&sections, "CSIZE").and_then(|value| value.parse::<u64>().ok());

    if filename.contains(['/', '\\'])
        || filename == "."
        || filename == ".."
        || filename.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "Arch 数据库条目 `{}` 的文件名 `{filename}` 非法",
            source.display()
        ));
    }
    if let Some(sha256) = &sha256 {
        if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "Arch 数据库条目 `{}` 的 SHA256 非法",
                source.display()
            ));
        }
    }
    if version.chars().any(char::is_whitespace) {
        return Err(format!(
            "Arch 数据库条目 `{}` 的版本 `{version}` 非法",
            source.display()
        ));
    }

    let depends = sections
        .get("DEPENDS")
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default();

    Ok(ArchRepoEntry {
        name,
        version,
        depends,
        filename,
        sha256,
        csize,
    })
}

/// 把 `desc` 拆成 `%KEY%` 到多行值的映射。
fn parse_sections(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;

    for line in text.lines() {
        if let Some(key) = line
            .strip_prefix('%')
            .and_then(|rest| rest.strip_suffix('%'))
        {
            if !key.is_empty()
                && key.chars().all(|character| {
                    character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
                })
            {
                current = Some(key.to_string());
                sections.entry(key.to_string()).or_default();
                continue;
            }
        }
        if let Some(key) = &current {
            sections
                .entry(key.clone())
                .or_default()
                .push(line.to_string());
        }
    }

    sections
}

/// 读取一个可选的单值字段。
fn optional_field(sections: &BTreeMap<String, Vec<String>>, key: &str) -> Option<String> {
    sections
        .get(key)
        .and_then(|lines| lines.first())
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
}

/// 读取一个必填的单值字段。
fn single_field(
    sections: &BTreeMap<String, Vec<String>>,
    key: &str,
    source: &Path,
) -> Result<String, Error> {
    optional_field(sections, key).ok_or_else(|| {
        format!(
            "Arch 数据库条目 `{}` 的 desc 缺少 %{key}%",
            source.display()
        )
    })
}

/// 把 Arch 依赖映射为内部 Capability，并保留原始依赖文本用于报错。
fn map_dependencies(
    depends: &[String],
    mapper: &DependencyMapper,
) -> Result<Vec<(String, Capability)>, Error> {
    depends
        .iter()
        .map(|dependency| {
            mapper
                .map_capability_quiet(dependency)
                .map(|capability| (dependency.clone(), capability))
        })
        .collect()
}

/// 找出第一条被基线明确判定为不满足的依赖。
///
/// 基线里没有的包名不做判断：它们可能由同一个仓库里的其它 Arch 包提供。
fn first_unsatisfied<'a>(
    dependencies: &'a [(String, Capability)],
    baseline: &SystemBaseline,
) -> Option<(&'a str, &'a Capability)> {
    for (source, mapped) in dependencies {
        let Some(available) = baseline.version_of(&mapped.name) else {
            continue;
        };
        if !mapped.matches(available) {
            return Some((source.as_str(), mapped));
        }
    }
    None
}

/// 下载/校验成功后的结果；`missing_sha` 用于稍后统一打印告警。
struct IngestSuccess {
    sha256: String,
    missing_sha: bool,
}

/// 包下载/校验失败的原因；ABI 拒绝单独分类，便于统计。
enum IngestError {
    Abi(String),
    Other(String),
}

/// 下载单个包，校验 SHA256，通过 ABI 扫描后原子落盘。
fn ingest_package(
    mirror_url: &str,
    repo_dir: &Path,
    entry: &ArchRepoEntry,
    guard: &AbiGuard,
    bar: &ProgressBar,
) -> Result<IngestSuccess, IngestError> {
    let destination = repo_dir.join(&entry.filename);
    let partial =
        PartialFile::new(repo_dir.join(format!(".{}.part-{}", entry.filename, unique_nonce())));

    let url = download::join_location(mirror_url, &entry.filename).map_err(IngestError::Other)?;
    download::to_file(&url, partial.path(), bar).map_err(IngestError::Other)?;

    let actual_sha256 = repo::sha256_file(partial.path()).map_err(IngestError::Other)?;
    let missing_sha = match &entry.sha256 {
        Some(expected) if !actual_sha256.eq_ignore_ascii_case(expected) => {
            return Err(IngestError::Other(format!(
                "包 `{}` 的 SHA256 不匹配: 期望 {expected}，实际 {actual_sha256}",
                entry.filename
            )));
        }
        Some(_) => false,
        None => true,
    };
    verify_arch_abi(partial.path(), &entry.name, guard).map_err(IngestError::Abi)?;

    if destination.exists() {
        fs::remove_file(&destination).map_err(|error| {
            IngestError::Other(format!(
                "替换旧包 `{}` 失败: {error}",
                destination.display()
            ))
        })?;
    }
    fs::rename(partial.path(), &destination).map_err(|error| {
        IngestError::Other(format!("保存包 `{}` 失败: {error}", destination.display()))
    })?;
    partial.keep();
    Ok(IngestSuccess {
        sha256: actual_sha256,
        missing_sha,
    })
}

/// 临时下载文件；离开作用域时自动删除，成功落盘后调用 [`Self::keep`]。
struct PartialFile {
    path: PathBuf,
    keep: bool,
}

impl PartialFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
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

    #[test]
    fn parses_multiline_depends_from_desc() {
        let text = "%FILENAME%\nfoo-1.0-1-x86_64.pkg.tar.zst\n\n%NAME%\nfoo\n\n%VERSION%\n1.0-1\n\n%DEPENDS%\nglibc\nlibfoo>=1.0\n\n%CSIZE%\n12345\n\n%SHA256SUM%\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        let entry = parse_desc(text, Path::new("foo-1.0-1/desc")).unwrap();

        assert_eq!(entry.name, "foo");
        assert_eq!(entry.version, "1.0-1");
        assert_eq!(entry.depends, ["glibc", "libfoo>=1.0"]);
        assert_eq!(entry.filename, "foo-1.0-1-x86_64.pkg.tar.zst");
        assert_eq!(entry.csize, Some(12345));
        assert!(entry.sha256.is_some());
    }

    #[test]
    fn derives_optional_fields_from_name_and_version() {
        let text = "%NAME%\nfoo\n\n%VERSION%\n1.0-1\n\n%DEPENDS%\n";
        let entry = parse_desc(text, Path::new("foo-1.0-1/desc")).unwrap();

        assert_eq!(entry.filename, "foo-1.0-1-x86_64.pkg.tar.zst");
        assert_eq!(entry.sha256, None);
        assert_eq!(entry.csize, None);
    }

    #[test]
    fn rejects_baseline_violating_dependency() {
        let mapper = DependencyMapper::default();
        let baseline = SystemBaseline::xros().unwrap();
        let dependencies = map_dependencies(&["glibc>=2.45".to_string()], &mapper).unwrap();

        let rejected = first_unsatisfied(&dependencies, &baseline).unwrap();

        assert_eq!(rejected.0, "glibc>=2.45");
        assert_eq!(rejected.1.to_string(), "glibc >= 2.45");
    }

    #[test]
    fn allows_dependency_at_or_below_the_baseline() {
        let mapper = DependencyMapper::default();
        let baseline = SystemBaseline::xros().unwrap();
        let dependencies =
            map_dependencies(&["glibc>=2.44".to_string(), "libfoo".to_string()], &mapper).unwrap();

        assert!(first_unsatisfied(&dependencies, &baseline).is_none());
    }
}
