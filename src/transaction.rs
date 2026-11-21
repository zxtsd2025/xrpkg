//! 文件系统事务：原子替换、升级/卸载快照和崩溃恢复。
//!
//! 发布一个文件时先在目标目录创建临时文件，完整写入内容并应用权限、属主和
//! 扩展属性，再用 `rename(2)` 原子替换目标。旧文件在替换前被硬链接（普通
//! 文件）或改名（符号链接）到 Rootfs 内的事务目录，失败时按日志反向恢复。
//!
//! 事务日志写在 `<rootfs>/.xrpkg-txn/<id>/journal`。进程崩溃后，下一次事务
//! 开始前会扫描遗留目录：日志带 `X`（已提交）或数据库已显示本次安装/卸载
//! 生效时，只清理备份；否则恢复旧文件。这样数据库提交与文件发布之间即使
//! 断电，也能在下次运行时收敛到一致状态。

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fs_meta;
use crate::state::LocalDb;
use crate::xpkg::Rootfs;

/// 事务目录名；放在 Rootfs 根下，保证 rename 与目标在同一文件系统。
pub(crate) const TXN_DIR: &str = ".xrpkg-txn";
const JOURNAL_NAME: &str = "journal";

/// 一条文件操作日志。
#[derive(Clone, Debug)]
enum Record {
    /// 发布新内容；`had_dest` 表示替换前目标是否已存在。
    Replace {
        relative: PathBuf,
        backup: String,
        had_dest: bool,
        temp: PathBuf,
    },
    /// 卸载文件；旧内容放在 `backup`。
    Remove { relative: PathBuf, backup: String },
    /// 本次事务安装的包，用于崩溃后判断数据库是否已提交。
    InstallPackage {
        name: String,
        version_raw: String,
        origin: String,
    },
    /// 本次事务卸载的包。
    RemovePackage { name: String },
    /// 事务已提交标记。
    Committed,
}

/// 一个 Rootfs 上的文件事务。
pub struct Transaction {
    root: PathBuf,
    dir: PathBuf,
    journal: File,
    records: Vec<Record>,
    counter: u64,
    finished: bool,
}

impl Transaction {
    /// 开始事务；开始前先恢复同一 Rootfs 上遗留的事务。
    pub fn begin(target: &Rootfs, db: Option<&LocalDb>) -> Result<Self, String> {
        recover(target.path(), db)?;

        let txn_root = target.path().join(TXN_DIR);
        fs::create_dir_all(&txn_root)
            .map_err(|error| format!("创建事务目录 `{}` 失败: {error}", txn_root.display()))?;

        let mut dir = txn_root.join(format!("{}-{}", std::process::id(), nonce()));
        for attempt in 0..16 {
            match fs::create_dir(&dir) {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 15 => {
                    dir = txn_root.join(format!("{}-{}-{attempt}", std::process::id(), nonce()));
                }
                Err(error) => {
                    return Err(format!("创建事务目录 `{}` 失败: {error}", dir.display()))
                }
            }
        }

        let journal_path = dir.join(JOURNAL_NAME);
        let mut journal = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&journal_path)
            .map_err(|error| format!("创建事务日志 `{}` 失败: {error}", journal_path.display()))?;
        journal
            .write_all(b"XRPKG-TXN-1\n")
            .and_then(|()| journal.sync_data())
            .map_err(|error| format!("写入事务日志头失败: {error}"))?;

        Ok(Self {
            root: target.path().to_path_buf(),
            dir,
            journal,
            records: Vec::new(),
            counter: 0,
            finished: false,
        })
    }

    /// 事务根目录（目标 Rootfs）。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 记录本次要安装的包；崩溃恢复时据此判断数据库是否已经提交。
    pub fn record_install(
        &mut self,
        name: &str,
        version_raw: &str,
        origin: &str,
    ) -> Result<(), String> {
        self.push(Record::InstallPackage {
            name: name.to_string(),
            version_raw: version_raw.to_string(),
            origin: origin.to_string(),
        })
    }

    /// 记录本次要卸载的包。
    pub fn record_remove(&mut self, name: &str) -> Result<(), String> {
        self.push(Record::RemovePackage {
            name: name.to_string(),
        })
    }

    /// 原子发布一个普通文件，并完整复制权限、属主和扩展属性。
    pub fn install_file(&mut self, relative: &Path, source: &Path) -> Result<(), String> {
        let source_metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("读取待发布文件 `{}` 失败: {error}", source.display()))?;
        if !source_metadata.is_file() || source_metadata.file_type().is_symlink() {
            return Err(format!("待发布路径 `{}` 不是普通文件", source.display()));
        }
        let metadata = fs_meta::read_metadata(source)?;

        let destination = self.root.join(relative);
        if let Some(parent) = relative.parent() {
            if !parent.as_os_str().is_empty() {
                crate::xpkg::ensure_directory(&self.root, parent)?;
            }
        }

        let (had_dest, backup) = self.prepare_backup(relative)?;
        let temp = self.temp_path("new");
        self.push(Record::Replace {
            relative: relative.to_path_buf(),
            backup: backup.clone(),
            had_dest,
            temp: temp.clone(),
        })?;
        if had_dest {
            self.create_backup(relative, &backup)?;
        }

        let result = (|| -> Result<(), String> {
            let mut input = File::open(source)
                .map_err(|error| format!("打开暂存文件 `{}` 失败: {error}", source.display()))?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temp)
                .map_err(|error| format!("创建临时文件 `{}` 失败: {error}", temp.display()))?;
            io::copy(&mut input, &mut output)
                .map_err(|error| format!("写入临时文件 `{}` 失败: {error}", temp.display()))?;
            output
                .sync_all()
                .map_err(|error| format!("同步临时文件 `{}` 失败: {error}", temp.display()))?;
            drop(output);

            fs_meta::apply_metadata(&temp, &metadata)?;
            File::open(&temp)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    format!("同步临时文件元数据 `{}` 失败: {error}", temp.display())
                })?;

            fs::rename(&temp, &destination).map_err(|error| {
                format!(
                    "原子发布 `{}` -> `{}` 失败: {error}",
                    temp.display(),
                    destination.display()
                )
            })?;
            fsync_dir(destination.parent());
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// 原子发布一个符号链接，并尽量保留属主。
    pub fn install_symlink(
        &mut self,
        relative: &Path,
        target: &Path,
        metadata: Option<&fs_meta::EntryMetadata>,
    ) -> Result<(), String> {
        let destination = self.root.join(relative);
        if let Some(parent) = relative.parent() {
            if !parent.as_os_str().is_empty() {
                crate::xpkg::ensure_directory(&self.root, parent)?;
            }
        }

        let (had_dest, backup) = self.prepare_backup(relative)?;
        let temp = self.temp_path("link");
        self.push(Record::Replace {
            relative: relative.to_path_buf(),
            backup: backup.clone(),
            had_dest,
            temp: temp.clone(),
        })?;
        if had_dest {
            self.create_backup(relative, &backup)?;
        }

        let result = (|| -> Result<(), String> {
            std::os::unix::fs::symlink(target, &temp).map_err(|error| {
                format!(
                    "创建临时符号链接 `{}` -> `{}` 失败: {error}",
                    temp.display(),
                    target.display()
                )
            })?;
            if let Some(metadata) = metadata {
                fs_meta::apply_symlink_metadata(&temp, metadata)?;
            }
            fs::rename(&temp, &destination).map_err(|error| {
                format!(
                    "原子发布符号链接 `{}` -> `{}` 失败: {error}",
                    temp.display(),
                    destination.display()
                )
            })?;
            fsync_dir(destination.parent());
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// 原子发布一个指向 Rootfs 内已有文件的硬链接。
    pub fn install_hardlink(
        &mut self,
        relative: &Path,
        origin_relative: &Path,
    ) -> Result<(), String> {
        let origin = self.root.join(origin_relative);
        let origin_metadata = fs::symlink_metadata(&origin)
            .map_err(|error| format!("读取硬链接源 `{}` 失败: {error}", origin.display()))?;
        if !origin_metadata.is_file() || origin_metadata.file_type().is_symlink() {
            return Err(format!("硬链接源 `{}` 不是普通文件", origin.display()));
        }

        let destination = self.root.join(relative);
        if let Some(parent) = relative.parent() {
            if !parent.as_os_str().is_empty() {
                crate::xpkg::ensure_directory(&self.root, parent)?;
            }
        }

        let (had_dest, backup) = self.prepare_backup(relative)?;
        let temp = self.temp_path("hard");
        self.push(Record::Replace {
            relative: relative.to_path_buf(),
            backup: backup.clone(),
            had_dest,
            temp: temp.clone(),
        })?;
        if had_dest {
            self.create_backup(relative, &backup)?;
        }

        let result = (|| -> Result<(), String> {
            fs::hard_link(&origin, &temp).map_err(|error| {
                format!(
                    "创建临时硬链接 `{}` -> `{}` 失败: {error}",
                    temp.display(),
                    origin.display()
                )
            })?;
            fs::rename(&temp, &destination).map_err(|error| {
                format!(
                    "原子发布硬链接 `{}` -> `{}` 失败: {error}",
                    temp.display(),
                    destination.display()
                )
            })?;
            fsync_dir(destination.parent());
            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    /// 把文件移入事务快照；返回旧文件是否存在。
    pub fn remove_file(&mut self, relative: &Path) -> Result<bool, String> {
        let destination = self.root.join(relative);
        let metadata = match fs::symlink_metadata(&destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "检查待卸载文件 `{}` 失败: {error}",
                    destination.display()
                ))
            }
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(format!("待卸载路径 `{}` 是目录", destination.display()));
        }

        let backup = self.next_backup_name();
        self.push(Record::Remove {
            relative: relative.to_path_buf(),
            backup: backup.clone(),
        })?;
        let backup_path = self.dir.join(&backup);
        fs::rename(&destination, &backup_path).map_err(|error| {
            format!(
                "快照文件 `{}` -> `{}` 失败: {error}",
                destination.display(),
                backup_path.display()
            )
        })?;
        fsync_dir(destination.parent());
        Ok(true)
    }

    /// 提交事务：写提交标记并清理备份。数据库必须已经成功提交。
    ///
    /// 标记或清理失败不会回滚已提交的文件；遗留目录会在下一次事务开始时由
    /// 数据库状态判定为已提交并清理。
    pub fn commit(mut self) {
        let _ = self.push(Record::Committed);
        self.finished = true;
        let _ = fs::remove_dir_all(&self.dir);
        remove_txn_root_if_empty(&self.root);
    }

    /// 显式回滚：按日志逆序恢复旧文件。
    pub fn rollback(mut self) -> Result<(), String> {
        self.finished = true;
        let result = rollback_records(&self.root, &self.dir, &self.records);
        if result.is_ok() {
            let _ = fs::remove_dir_all(&self.dir);
            remove_txn_root_if_empty(&self.root);
        }
        result
    }

    fn temp_path(&mut self, kind: &str) -> PathBuf {
        self.counter += 1;
        self.root.join(format!(
            ".xrpkg-{kind}-{}-{}",
            std::process::id(),
            self.counter
        ))
    }

    /// 检查旧目标并分配备份名；真正的备份动作由 [`Self::create_backup`] 完成。
    fn prepare_backup(&mut self, relative: &Path) -> Result<(bool, String), String> {
        let destination = self.root.join(relative);
        let metadata = match fs::symlink_metadata(&destination) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((false, String::new()))
            }
            Err(error) => {
                return Err(format!(
                    "检查发布目标 `{}` 失败: {error}",
                    destination.display()
                ))
            }
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            return Err(format!("发布目标 `{}` 是目录", destination.display()));
        }
        Ok((true, self.next_backup_name()))
    }

    /// 备份旧目标：普通文件用硬链接，符号链接用改名，避免复制内容和元数据。
    fn create_backup(&mut self, relative: &Path, backup: &str) -> Result<(), String> {
        let destination = self.root.join(relative);
        let backup_path = self.dir.join(backup);
        let metadata = fs::symlink_metadata(&destination)
            .map_err(|error| format!("检查发布目标 `{}` 失败: {error}", destination.display()))?;
        if metadata.file_type().is_symlink() {
            fs::rename(&destination, &backup_path).map_err(|error| {
                format!(
                    "备份符号链接 `{}` -> `{}` 失败: {error}",
                    destination.display(),
                    backup_path.display()
                )
            })
        } else {
            fs::hard_link(&destination, &backup_path).map_err(|error| {
                format!(
                    "备份文件 `{}` -> `{}` 失败: {error}",
                    destination.display(),
                    backup_path.display()
                )
            })
        }
    }

    fn next_backup_name(&mut self) -> String {
        self.counter += 1;
        format!("backup-{}", self.counter)
    }

    fn push(&mut self, record: Record) -> Result<(), String> {
        let line = encode_record(&record);
        self.journal
            .write_all(line.as_bytes())
            .and_then(|()| self.journal.sync_data())
            .map_err(|error| format!("写入事务日志失败: {error}"))?;
        self.records.push(record);
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if let Err(error) = rollback_records(&self.root, &self.dir, &self.records) {
            eprintln!("warning: 事务自动回滚失败: {error}");
        } else {
            let _ = fs::remove_dir_all(&self.dir);
            remove_txn_root_if_empty(&self.root);
        }
    }
}

/// 恢复 Rootfs 上遗留的事务。
pub fn recover(root: &Path, db: Option<&LocalDb>) -> Result<(), String> {
    let txn_root = root.join(TXN_DIR);
    let entries = match fs::read_dir(&txn_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "读取事务目录 `{}` 失败: {error}",
                txn_root.display()
            ))
        }
    };

    for entry in entries {
        let entry = entry.map_err(|error| format!("读取事务目录项失败: {error}"))?;
        let dir = entry.path();
        if !entry
            .file_type()
            .map_err(|error| format!("读取事务目录项类型失败: {error}"))?
            .is_dir()
        {
            continue;
        }
        recover_one(root, &dir, db)?;
    }
    remove_txn_root_if_empty(root);
    Ok(())
}

fn recover_one(root: &Path, dir: &Path, db: Option<&LocalDb>) -> Result<(), String> {
    let journal_path = dir.join(JOURNAL_NAME);
    let text = match fs::read_to_string(&journal_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let _ = fs::remove_dir_all(dir);
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "读取事务日志 `{}` 失败: {error}",
                journal_path.display()
            ))
        }
    };

    let records = parse_journal(&text)?;
    let committed = records
        .iter()
        .any(|record| matches!(record, Record::Committed))
        || database_already_committed(&records, db)?;
    if committed {
        fs::remove_dir_all(dir)
            .map_err(|error| format!("清理已提交事务 `{}` 失败: {error}", dir.display()))?;
        return Ok(());
    }

    rollback_records(root, dir, &records)?;
    fs::remove_dir_all(dir)
        .map_err(|error| format!("清理回滚后的事务 `{}` 失败: {error}", dir.display()))
}

/// 根据数据库状态判断：崩溃发生在数据库提交之后吗？
fn database_already_committed(records: &[Record], db: Option<&LocalDb>) -> Result<bool, String> {
    let installs = records
        .iter()
        .filter_map(|record| match record {
            Record::InstallPackage {
                name, version_raw, ..
            } => Some((name, version_raw)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let removes = records
        .iter()
        .filter_map(|record| match record {
            Record::RemovePackage { name } => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    if installs.is_empty() && removes.is_empty() {
        return Ok(false);
    }

    let Some(db) = db else {
        return Ok(false);
    };
    let installed = db.list_installed()?;
    for (name, version_raw) in installs {
        if !installed
            .iter()
            .any(|package| &package.name == name && &package.version_raw == version_raw)
        {
            return Ok(false);
        }
    }
    for name in removes {
        if installed.iter().any(|package| &package.name == name) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn rollback_records(root: &Path, dir: &Path, records: &[Record]) -> Result<(), String> {
    let mut errors = Vec::new();
    for record in records.iter().rev() {
        let result = match record {
            Record::Replace {
                relative,
                backup,
                had_dest,
                temp,
            } => {
                let _ = fs::remove_file(temp);
                let destination = root.join(relative);
                if *had_dest {
                    let backup_path = dir.join(backup);
                    if backup_path.exists() {
                        let _ = remove_non_directory(&destination);
                        fs::rename(&backup_path, &destination).map_err(|error| {
                            format!("恢复 `{}` 失败: {error}", destination.display())
                        })
                    } else {
                        Ok(())
                    }
                } else {
                    remove_non_directory(&destination)
                }
            }
            Record::Remove { relative, backup } => {
                let destination = root.join(relative);
                let backup_path = dir.join(backup);
                if backup_path.exists() {
                    let _ = remove_non_directory(&destination);
                    fs::rename(&backup_path, &destination).map_err(|error| {
                        format!("恢复卸载文件 `{}` 失败: {error}", destination.display())
                    })
                } else {
                    Ok(())
                }
            }
            Record::InstallPackage { .. } | Record::RemovePackage { .. } | Record::Committed => {
                Ok(())
            }
        };
        if let Err(error) = result {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// 删除文件或符号链接；目标是目录时拒绝。
fn remove_non_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Err(format!("回滚目标 `{}` 是目录", path.display()))
        }
        Ok(_) => fs::remove_file(path)
            .map_err(|error| format!("回滚删除 `{}` 失败: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("回滚检查 `{}` 失败: {error}", path.display())),
    }
}

/// 事务目录为空时删除 `.xrpkg-txn`，不给目标系统留下空目录。
fn remove_txn_root_if_empty(root: &Path) {
    let txn_root = root.join(TXN_DIR);
    if let Ok(mut entries) = fs::read_dir(&txn_root) {
        if entries.next().is_none() {
            let _ = fs::remove_dir(&txn_root);
        }
    }
}

fn fsync_dir(directory: Option<&Path>) {
    if let Some(directory) = directory {
        if let Ok(file) = File::open(directory) {
            let _ = file.sync_all();
        }
    }
}

fn encode_record(record: &Record) -> String {
    match record {
        Record::Replace {
            relative,
            backup,
            had_dest,
            temp,
        } => format!(
            "R {} {} {} {}\n",
            hex_encode(relative.as_os_str().as_bytes()),
            hex_encode(backup.as_bytes()),
            u8::from(*had_dest),
            hex_encode(temp.as_os_str().as_bytes())
        ),
        Record::Remove { relative, backup } => format!(
            "D {} {}\n",
            hex_encode(relative.as_os_str().as_bytes()),
            hex_encode(backup.as_bytes())
        ),
        Record::InstallPackage {
            name,
            version_raw,
            origin,
        } => format!(
            "P {} {} {}\n",
            hex_encode(name.as_bytes()),
            hex_encode(version_raw.as_bytes()),
            hex_encode(origin.as_bytes())
        ),
        Record::RemovePackage { name } => {
            format!("U {}\n", hex_encode(name.as_bytes()))
        }
        Record::Committed => "X\n".to_string(),
    }
}

fn parse_journal(text: &str) -> Result<Vec<Record>, String> {
    let mut records = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line == "XRPKG-TXN-1" {
            continue;
        }
        let mut parts = line.split(' ');
        let kind = parts
            .next()
            .ok_or_else(|| "事务日志行缺少类型".to_string())?;
        let record = match kind {
            "R" => {
                let relative = decode_path(parts.next())?;
                let backup = decode_text(parts.next())?;
                let had_dest = match parts.next() {
                    Some("1") => true,
                    Some("0") => false,
                    _ => return Err("事务日志的 had_dest 字段非法".to_string()),
                };
                let temp = decode_path(parts.next())?;
                Record::Replace {
                    relative,
                    backup,
                    had_dest,
                    temp,
                }
            }
            "D" => Record::Remove {
                relative: decode_path(parts.next())?,
                backup: decode_text(parts.next())?,
            },
            "P" => Record::InstallPackage {
                name: decode_text(parts.next())?,
                version_raw: decode_text(parts.next())?,
                origin: decode_text(parts.next())?,
            },
            "U" => Record::RemovePackage {
                name: decode_text(parts.next())?,
            },
            "X" => Record::Committed,
            other => return Err(format!("未知事务日志类型 `{other}`")),
        };
        records.push(record);
    }
    Ok(records)
}

fn decode_path(value: Option<&str>) -> Result<PathBuf, String> {
    let bytes = decode_hex(value.ok_or_else(|| "事务日志缺少路径".to_string())?)?;
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn decode_text(value: Option<&str>) -> Result<String, String> {
    let bytes = decode_hex(value.ok_or_else(|| "事务日志缺少字段".to_string())?)?;
    String::from_utf8(bytes).map_err(|error| format!("事务日志字段不是 UTF-8: {error}"))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("事务日志十六进制字段长度为奇数".to_string());
    }
    let mut output = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("非法十六进制字符 `{}`", byte as char)),
    }
}

/// 收集一组文件的父目录，供提交后清理空目录。
pub fn parent_directories(files: impl IntoIterator<Item = PathBuf>) -> BTreeSet<PathBuf> {
    let mut directories = BTreeSet::new();
    for file in files {
        let mut current = file.parent();
        while let Some(directory) = current {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_path_buf());
            current = directory.parent();
        }
    }
    directories
}

/// 提交后由深到浅删除空目录；非空目录保留。
pub fn remove_empty_directories(
    root: &Path,
    directories: &BTreeSet<PathBuf>,
) -> Result<usize, String> {
    let mut ordered = directories.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|directory| std::cmp::Reverse(directory.components().count()));
    let mut removed = 0;
    for relative in ordered {
        let directory = root.join(relative);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "检查空目录 `{}` 失败: {error}",
                    directory.display()
                ))
            }
        }
        let mut entries = fs::read_dir(&directory)
            .map_err(|error| format!("读取目录 `{}` 失败: {error}", directory.display()))?;
        if entries.next().is_some() {
            continue;
        }
        fs::remove_dir(&directory)
            .map_err(|error| format!("删除空目录 `{}` 失败: {error}", directory.display()))?;
        removed += 1;
    }
    Ok(removed)
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_round_trips_arbitrary_paths() {
        let record = Record::Replace {
            relative: PathBuf::from("usr/bin/a b"),
            backup: "backup-1".to_string(),
            had_dest: true,
            temp: PathBuf::from("usr/bin/.xrpkg-new-1"),
        };
        let text = encode_record(&record);
        let parsed = parse_journal(&format!("XRPKG-TXN-1\n{text}")).unwrap();
        assert_eq!(parsed.len(), 1);
        match &parsed[0] {
            Record::Replace {
                relative,
                backup,
                had_dest,
                temp,
            } => {
                assert_eq!(relative, Path::new("usr/bin/a b"));
                assert_eq!(backup, "backup-1");
                assert!(had_dest);
                assert_eq!(temp, Path::new("usr/bin/.xrpkg-new-1"));
            }
            other => panic!("unexpected record {other:?}"),
        }
    }

    #[test]
    fn journal_rejects_corruption() {
        assert!(parse_journal("R zz 00 1 00\n").is_err());
        assert!(parse_journal("R 00 00 2 00\n").is_err());
    }

    #[test]
    fn no_recovery_needed_for_missing_directory() {
        let temp = tempfile::TempDir::new().unwrap();
        recover(temp.path(), None).unwrap();
    }
}
