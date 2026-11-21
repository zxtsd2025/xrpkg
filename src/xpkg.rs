//! `.xpkg` 包的读取与安全解包。
//!
//! 格式：ZSTD 压缩的 TAR。包根必须有 `metadata.toml`，实际文件放在 `files/` 下。
//! 解包时允许普通文件、目录、符号链接和硬链接；所有路径和链接目标都做 Rootfs 逃逸检查。

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::capability::{Capability, Version};
use crate::fs_meta::{self, EntryMetadata};
use crate::hooks::{HookScript, ALL_PHASES};
use crate::solver::SelectedPackage;

/// metadata.toml 的大小上限，避免异常包耗尽内存。
const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// 从 metadata.toml 解析出的包元数据。
#[derive(Clone, Debug)]
pub struct PackageMetadata {
    pub name: String,
    pub version: Version,
    pub requires: Vec<Capability>,
}

/// 受保护的安装根目录。
#[derive(Clone, Debug)]
pub struct Rootfs {
    path: PathBuf,
}

impl Rootfs {
    /// 创建并规范化根目录；拒绝把宿主根目录 `/` 当作安装目标。
    pub fn prepare(path: impl AsRef<Path>) -> Result<Self, String> {
        let requested = path.as_ref();
        if requested.as_os_str().is_empty() {
            return Err("Rootfs 路径不能为空".to_string());
        }

        fs::create_dir_all(requested)
            .map_err(|error| format!("创建 Rootfs `{}` 失败: {error}", requested.display()))?;
        let path = fs::canonicalize(requested)
            .map_err(|error| format!("规范化 Rootfs `{}` 失败: {error}", requested.display()))?;

        if path == Path::new("/") {
            return Err("拒绝把宿主根目录 `/` 作为安装 Rootfs".to_string());
        }

        Ok(Self { path })
    }

    /// 返回规范化后的根目录路径。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 读取并解析包内的 metadata.toml。
pub fn read_metadata(archive: impl AsRef<Path>) -> Result<PackageMetadata, String> {
    let archive = archive.as_ref();
    let file = File::open(archive)
        .map_err(|error| format!("打开 .xpkg `{}` 失败: {error}", archive.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    let mut tar = tar::Archive::new(decoder);

    let mut metadata_text = None;
    for entry in tar
        .entries()
        .map_err(|error| format!("读取 TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 TAR 条目路径失败: {error}"))?
            .into_owned();

        if path == Path::new("metadata.toml") {
            if metadata_text.is_some() {
                return Err("包内出现重复的 metadata.toml".to_string());
            }
            if !entry.header().entry_type().is_file() {
                return Err("metadata.toml 必须是普通文件".to_string());
            }
            if entry.size() > MAX_METADATA_BYTES {
                return Err(format!(
                    "metadata.toml 超过 {} 字节上限",
                    MAX_METADATA_BYTES
                ));
            }

            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|error| format!("读取 metadata.toml 失败: {error}"))?;
            metadata_text = Some(text);
        }
    }

    let text = metadata_text.ok_or_else(|| "包内缺少 metadata.toml".to_string())?;
    parse_metadata(&text)
}

/// 读取 `.xpkg` 根目录中的生命周期脚本。
pub fn read_hooks(archive: impl AsRef<Path>) -> Result<Vec<HookScript>, String> {
    let archive = archive.as_ref();
    let file = File::open(archive)
        .map_err(|error| format!("打开 .xpkg `{}` 失败: {error}", archive.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    let mut tar = tar::Archive::new(decoder);
    let mut hooks = Vec::new();
    let mut seen = BTreeSet::new();

    for entry in tar
        .entries()
        .map_err(|error| format!("读取 TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 TAR 条目路径失败: {error}"))?
            .into_owned();
        let Some(phase) = hook_phase(&path) else {
            continue;
        };
        if !entry.header().entry_type().is_file() {
            return Err(format!("hook `{}` 必须是普通文件", path.display()));
        }
        if entry.size() > MAX_METADATA_BYTES {
            return Err(format!("hook `{}` 超过 1 MiB", path.display()));
        }
        if !seen.insert(phase) {
            return Err(format!("包内出现重复 hook `{phase}`"));
        }
        let mut script = Vec::new();
        entry
            .read_to_end(&mut script)
            .map_err(|error| format!("读取 hook `{phase}` 失败: {error}"))?;
        hooks.push(HookScript {
            phase: phase.to_string(),
            script,
        });
    }
    Ok(hooks)
}

/// 根目录脚本名到阶段的映射。
fn hook_phase(path: &Path) -> Option<&'static str> {
    for phase in ALL_PHASES {
        if path == Path::new(&format!("{phase}.sh")) {
            return Some(phase);
        }
    }
    None
}

/// 校验 metadata 中的包名和版本，然后把 files/ 安全解包到 Rootfs。
pub fn install_package(
    archive: impl AsRef<Path>,
    expected: &SelectedPackage,
    rootfs: &Rootfs,
) -> Result<Vec<String>, String> {
    let archive = archive.as_ref();
    let metadata = read_metadata(archive)?;

    if metadata.name != expected.name {
        return Err(format!(
            "包 `{}` 的 metadata 包名是 `{}`，但计划要求 `{}`",
            archive.display(),
            metadata.name,
            expected.name
        ));
    }
    if metadata.version != expected.version {
        return Err(format!(
            "包 `{}` 的 metadata 版本是 {}，但计划要求版本 {}",
            archive.display(),
            metadata.version,
            expected.version
        ));
    }

    extract_files(archive, rootfs)
}

/// 把暂存 Rootfs 中通过 ABI 检查的文件发布到真实 Rootfs。
///
/// 兼容测试和独立工具使用的一次性接口；正式安装应使用
/// [`publish_files_txn`]，把发布纳入更大的数据库事务。
pub fn publish_files(staging: &Rootfs, target: &Rootfs, files: &[String]) -> Result<(), String> {
    let mut transaction = crate::transaction::Transaction::begin(target, None)?;
    publish_files_txn(&mut transaction, staging, files)?;
    transaction.commit();
    Ok(())
}

/// 事务化发布：每个文件先写临时文件，再原子 rename 到目标。
///
/// 普通文件保留硬链接关系；符号链接在第二遍发布，确保其指向的文件已经存在。
pub fn publish_files_txn(
    transaction: &mut crate::transaction::Transaction,
    staging: &Rootfs,
    files: &[String],
) -> Result<(), String> {
    let target = transaction.root().to_path_buf();
    let mut regular_files = Vec::new();
    let mut symlinks = Vec::new();
    for file in files {
        let relative = sanitize_relative(Path::new(file))
            .map_err(|error| format!("文件清单中的路径 `{file}` 非法: {error}"))?;
        if relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == crate::transaction::TXN_DIR)
        {
            return Err(format!("文件路径 `{file}` 占用了保留目录 `.xrpkg-txn`"));
        }
        let source = staging.path().join(&relative);
        let metadata = fs::symlink_metadata(&source)
            .map_err(|error| format!("读取暂存文件 `{}` 失败: {error}", source.display()))?;
        if metadata.file_type().is_symlink() {
            let link_metadata = fs_meta::read_metadata(&source)?;
            symlinks.push((relative, source, link_metadata));
        } else if metadata.is_file() {
            regular_files.push((relative, source, metadata));
        } else {
            return Err(format!(
                "暂存路径 `{}` 不是文件或符号链接",
                source.display()
            ));
        }
    }

    // 第一遍发布普通文件，并保留硬链接关系。
    let mut hardlinks: HashMap<(u64, u64), PathBuf> = HashMap::new();
    for (relative, source, source_metadata) in regular_files {
        if let Some(parent) = relative.parent() {
            if !parent.as_os_str().is_empty() {
                ensure_directory(&target, parent)?;
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            if source_metadata.nlink() > 1 {
                let key = (source_metadata.dev(), source_metadata.ino());
                if let Some(origin) = hardlinks.get(&key) {
                    transaction.install_hardlink(&relative, origin)?;
                    continue;
                }
                hardlinks.insert(key, relative.clone());
            }
        }

        transaction.install_file(&relative, &source)?;
    }

    // 第二遍发布符号链接，确保链接指向的普通文件已经存在。
    for (relative, source, metadata) in symlinks {
        let link_target = fs::read_link(&source)
            .map_err(|error| format!("读取暂存符号链接 `{}` 失败: {error}", source.display()))?;
        validate_symlink_target(&target, &relative, &link_target)?;
        transaction.install_symlink(&relative, &link_target, Some(&metadata))?;
    }

    Ok(())
}

/// 检查发布目标；旧符号链接会被删除，但绝不通过链接写入其目标。
fn ensure_publish_destination(destination: &Path) -> Result<(), String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // 只删除链接本身，随后在同一路径创建新文件/链接，链接目标不受影响。
            fs::remove_file(destination).map_err(|error| {
                format!("删除旧符号链接 `{}` 失败: {error}", destination.display())
            })
        }
        Ok(metadata) if metadata.is_dir() => {
            Err(format!("目标文件 `{}` 是目录", destination.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "检查目标文件 `{}` 失败: {error}",
            destination.display()
        )),
    }
}

/// metadata.toml 的 TOML 结构。
#[derive(Debug, Deserialize)]
struct MetadataDocument {
    name: String,
    version: String,
    #[serde(default)]
    requires: Vec<String>,
}

/// 把 metadata 字符串转换成结构化元数据。
fn parse_metadata(text: &str) -> Result<PackageMetadata, String> {
    let document: MetadataDocument =
        toml::from_str(text).map_err(|error| format!("解析 metadata.toml 失败: {error}"))?;

    if document.name.is_empty() {
        return Err("metadata.toml 的 name 不能为空".to_string());
    }
    let version = Version::parse(&document.version)
        .map_err(|error| format!("metadata.toml 的 version 无效: {error}"))?;
    let requires = document
        .requires
        .iter()
        .map(|text| {
            Capability::parse(text)
                .map_err(|error| format!("metadata.toml 的 requires 项 `{text}` 无效: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(PackageMetadata {
        name: document.name,
        version,
        requires,
    })
}

/// 第二次遍历 TAR，只接受 metadata.toml 和 files/ 下的条目。
fn extract_files(archive: &Path, rootfs: &Rootfs) -> Result<Vec<String>, String> {
    let file = File::open(archive)
        .map_err(|error| format!("打开 .xpkg `{}` 失败: {error}", archive.display()))?;
    let decoder = zstd::stream::read::Decoder::new(file)
        .map_err(|error| format!("创建 ZSTD 解码器失败: {error}"))?;
    let mut tar = tar::Archive::new(decoder);

    let mut installed = BTreeSet::new();
    let mut seen_paths = BTreeSet::new();

    for entry in tar
        .entries()
        .map_err(|error| format!("读取 TAR 目录失败: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("读取 TAR 条目失败: {error}"))?;
        let path = entry
            .path()
            .map_err(|error| format!("读取 TAR 条目路径失败: {error}"))?
            .into_owned();

        if path == Path::new("metadata.toml") || hook_phase(&path).is_some() {
            continue;
        }

        let relative = path.strip_prefix("files").map_err(|_| {
            format!(
                "包内出现非法路径 `{}`；只允许 metadata.toml 和 files/ 下的文件",
                path.display()
            )
        })?;
        let entry_type = entry.header().entry_type();

        if relative.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err(format!(
                "路径 `{}` 不是合法的 files/ 目录条目",
                path.display()
            ));
        }

        let relative = sanitize_relative(relative)
            .map_err(|error| format!("包内出现非法路径 `{}`: {error}", path.display()))?;

        if !seen_paths.insert(relative.clone()) {
            return Err(format!("包内出现重复路径 `{}`", path.display()));
        }
        if entry_type.is_dir() {
            let metadata = EntryMetadata::from_tar(&mut entry)?;
            ensure_directory_with_metadata(rootfs.path(), &relative, &metadata)?;
        } else if entry_type.is_file() {
            let size = entry.size();
            let metadata = EntryMetadata::from_tar(&mut entry)?;
            write_file(rootfs.path(), &relative, &mut entry, size, &metadata)?;
            installed.insert(path_for_database(&relative));
        } else if entry_type == tar::EntryType::Symlink {
            let metadata = EntryMetadata::from_tar(&mut entry)?;
            let link_target = entry
                .link_name()
                .map_err(|error| format!("读取符号链接目标 `{}` 失败: {error}", path.display()))?
                .ok_or_else(|| format!("符号链接 `{}` 缺少目标", path.display()))?;
            create_symlink(rootfs.path(), &relative, link_target.as_ref())?;
            fs_meta::apply_symlink_metadata(&rootfs.path().join(&relative), &metadata)?;
            installed.insert(path_for_database(&relative));
        } else if entry_type == tar::EntryType::Link {
            let link_name = entry
                .link_name()
                .map_err(|error| format!("读取硬链接目标 `{}` 失败: {error}", path.display()))?
                .ok_or_else(|| format!("硬链接 `{}` 缺少目标", path.display()))?;
            let link_relative = link_name
                .strip_prefix("files")
                .map_err(|_| format!("硬链接目标 `{}` 不在 files/ 下", link_name.display()))?;
            let link_relative = sanitize_relative(link_relative)
                .map_err(|error| format!("硬链接目标 `{}` 非法: {error}", link_name.display()))?;
            if !seen_paths.contains(&link_relative) {
                return Err(format!(
                    "硬链接 `{}` 的目标 `{}` 尚未安装",
                    path.display(),
                    link_name.display()
                ));
            }
            create_hard_link(rootfs.path(), &relative, &link_relative)?;
            installed.insert(path_for_database(&relative));
        } else {
            return Err(format!("拒绝非普通文件/目录条目 `{}`", path.display()));
        }
    }

    if installed.is_empty() {
        return Err("包内 files/ 下没有任何文件".to_string());
    }

    Ok(installed.into_iter().collect())
}

/// 创建一个安全的相对符号链接。
pub(crate) fn create_symlink(root: &Path, relative: &Path, target: &Path) -> Result<(), String> {
    validate_symlink_target(root, relative, target)?;
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_directory(root, parent)?;
        }
    }
    let destination = root.join(relative);
    ensure_publish_destination(&destination)?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, &destination).map_err(|error| {
            format!(
                "创建符号链接 `{}` -> `{}` 失败: {error}",
                destination.display(),
                target.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (target, destination);
        Err("当前平台不支持符号链接安装".to_string())
    }
}

/// 创建一个指向 Rootfs 内普通文件的硬链接。
pub(crate) fn create_hard_link(
    root: &Path,
    relative: &Path,
    target_relative: &Path,
) -> Result<(), String> {
    let target = root.join(target_relative);
    let metadata = fs::symlink_metadata(&target)
        .map_err(|error| format!("读取硬链接目标 `{}` 失败: {error}", target.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "硬链接目标 `{}` 不是普通文件，拒绝创建",
            target.display()
        ));
    }

    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_directory(root, parent)?;
        }
    }
    let destination = root.join(relative);
    ensure_publish_destination(&destination)?;
    fs::hard_link(&target, &destination).map_err(|error| {
        format!(
            "创建硬链接 `{}` -> `{}` 失败: {error}",
            destination.display(),
            target.display()
        )
    })
}

/// 校验符号链接目标：必须是相对路径，且解析后不能逃出 Rootfs。
pub(crate) fn validate_symlink_target(
    root: &Path,
    link_relative: &Path,
    target: &Path,
) -> Result<(), String> {
    if target.as_os_str().is_empty() {
        return Err("符号链接目标不能为空".to_string());
    }
    if target.is_absolute() {
        return Err(format!(
            "符号链接 `{}` 的目标 `{}` 是绝对路径，拒绝安装",
            link_relative.display(),
            target.display()
        ));
    }

    let resolved = resolve_link_target(link_relative, target)?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("规范化 Rootfs `{}` 失败: {error}", root.display()))?;
    let candidate = canonical_root.join(&resolved);
    ensure_symlink_target_within(&canonical_root, &candidate, link_relative, target)
}

/// 校验符号链接目标不会逃出 Rootfs；目标可以稍后由同一个包创建。
fn ensure_symlink_target_within(
    canonical_root: &Path,
    candidate: &Path,
    link_relative: &Path,
    target: &Path,
) -> Result<(), String> {
    match fs::symlink_metadata(candidate) {
        Ok(metadata) if metadata.file_type().is_symlink() => match fs::canonicalize(candidate) {
            Ok(canonical) => ensure_within(canonical_root, &canonical, link_relative, target),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ensure_existing_ancestor_within(canonical_root, candidate.parent(), link_relative)
            }
            Err(error) => Err(format!(
                "解析符号链接目标 `{}` 失败: {error}",
                candidate.display()
            )),
        },
        Ok(_) => {
            let canonical = fs::canonicalize(candidate).map_err(|error| {
                format!("解析符号链接目标 `{}` 失败: {error}", candidate.display())
            })?;
            ensure_within(canonical_root, &canonical, link_relative, target)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // 悬空链接是合法的：只检查最近的已存在祖先是否仍在 Rootfs 内。
            ensure_existing_ancestor_within(canonical_root, candidate.parent(), link_relative)
        }
        Err(error) => Err(format!(
            "检查符号链接目标 `{}` 失败: {error}",
            candidate.display()
        )),
    }
}

/// 从给定路径向上找到第一个存在的祖先，规范化后必须仍在 Rootfs 内。
fn ensure_existing_ancestor_within(
    canonical_root: &Path,
    start: Option<&Path>,
    link_relative: &Path,
) -> Result<(), String> {
    let mut ancestor = start;
    while let Some(path) = ancestor {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                let canonical = fs::canonicalize(path).map_err(|error| {
                    format!("解析符号链接祖先 `{}` 失败: {error}", path.display())
                })?;
                if canonical.starts_with(canonical_root) {
                    return Ok(());
                }
                return Err(format!(
                    "符号链接 `{}` 的目标祖先 `{}` 在 Rootfs 外",
                    link_relative.display(),
                    path.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => ancestor = path.parent(),
            Err(error) => {
                return Err(format!(
                    "检查符号链接目标祖先 `{}` 失败: {error}",
                    path.display()
                ))
            }
        }
    }
    Ok(())
}

/// 规范化后的目标路径必须仍在 Rootfs 内。
fn ensure_within(
    canonical_root: &Path,
    canonical: &Path,
    link_relative: &Path,
    target: &Path,
) -> Result<(), String> {
    if canonical.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(format!(
            "符号链接 `{}` 的目标 `{}` 解析到 Rootfs 外",
            link_relative.display(),
            target.display()
        ))
    }
}

/// 把链接所在目录与相对目标拼接，并做纯词法 `..` 归一化。
fn resolve_link_target(link_relative: &Path, target: &Path) -> Result<PathBuf, String> {
    let mut parts = Vec::new();
    if let Some(parent) = link_relative.parent() {
        for component in parent.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_os_string()),
                _ => return Err("符号链接路径包含非法组件".to_string()),
            }
        }
    }
    for component in target.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(format!("符号链接目标 `{}` 逃出 Rootfs", target.display()));
                }
            }
            _ => {
                return Err(format!(
                    "符号链接目标 `{}` 包含绝对路径或盘符",
                    target.display()
                ))
            }
        }
    }

    let mut resolved = PathBuf::new();
    for part in parts {
        resolved.push(part);
    }
    Ok(resolved)
}

/// 只允许普通路径组件，拒绝 `..`、`.`、绝对路径和盘符。
pub fn sanitize_relative(path: &Path) -> Result<PathBuf, String> {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            _ => return Err("包含 `.`、`..`、根目录或盘符等非法路径组件".to_string()),
        }
    }

    if cleaned.as_os_str().is_empty() {
        Err("files/ 后没有文件路径".to_string())
    } else {
        Ok(cleaned)
    }
}

/// 逐级创建目录，拒绝已经存在的符号链接或非目录组件。
pub(crate) fn ensure_directory(root: &Path, relative: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("目录路径包含非法组件".to_string());
        };
        current.push(part);

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "目标路径组件 `{}` 是符号链接，拒绝跟随",
                    current.display()
                ))
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("目标路径组件 `{}` 不是目录", current.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|error| format!("创建目录 `{}` 失败: {error}", current.display()))?;
                set_directory_mode(&current)?;
            }
            Err(error) => return Err(format!("检查路径 `{}` 失败: {error}", current.display())),
        }
    }

    Ok(())
}

/// 创建目录并应用包内记录的权限位。
pub(crate) fn ensure_directory_with_metadata(
    root: &Path,
    relative: &Path,
    metadata: &EntryMetadata,
) -> Result<(), String> {
    ensure_directory(root, relative)?;
    fs_meta::apply_metadata(&root.join(relative), metadata)
}

/// 安全写入普通文件，并拒绝覆盖已有符号链接。
pub(crate) fn write_file(
    root: &Path,
    relative: &Path,
    entry: &mut impl Read,
    expected_size: u64,
    metadata: &EntryMetadata,
) -> Result<(), String> {
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_directory(root, parent)?;
        }
    }

    let target = root.join(relative);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "目标文件 `{}` 是符号链接，拒绝覆盖",
                target.display()
            ))
        }
        Ok(metadata) if metadata.is_dir() => {
            return Err(format!("目标文件 `{}` 是目录", target.display()))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查目标文件 `{}` 失败: {error}", target.display())),
    }

    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&target)
        .map_err(|error| format!("创建文件 `{}` 失败: {error}", target.display()))?;

    let copied = io::copy(entry, &mut output)
        .map_err(|error| format!("写入文件 `{}` 失败: {error}", target.display()))?;
    if copied != expected_size {
        return Err(format!(
            "文件 `{}` 数据长度不符：TAR 声明 {expected_size}，实际 {copied}",
            target.display()
        ));
    }

    fs_meta::apply_metadata(&target, metadata)
}

/// 把归档里的普通相对路径转换成数据库中的 `/` 分隔路径。
fn path_for_database(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(unix)]
/// 新建目录统一设置为 0755。
fn set_directory_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("设置目录权限 `{}` 失败: {error}", path.display()))
}

#[cfg(not(unix))]
/// 非 Unix 平台不处理权限位。
fn set_directory_mode(_path: &Path) -> Result<(), String> {
    Ok(())
}
