//! 本地已安装状态数据库。
//!
//! 记录已安装包、版本、安装时间，以及每个包落盘的文件清单。

use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};

use crate::capability::Version;
use crate::hooks::HookScript;
use crate::solver::SelectedPackage;

/// 数据库中的一条已安装包记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledPackage {
    pub name: String,
    /// 给 native 求解器使用的数字版本；外部包无法解析时回退为 0。
    pub version: Version,
    /// 数据库中的原始版本字符串，例如 Debian 的 `1.0-1`。
    pub version_raw: String,
    pub installed_at: String,
    pub origin: String,
}

/// 一次提交中的包记录和它的文件清单。
#[derive(Clone, Debug)]
pub struct PackageInstall {
    pub package: SelectedPackage,
    pub version_raw: String,
    pub origin: String,
    pub files: Vec<String>,
    pub hooks: Vec<HookScript>,
}

/// 本地 SQLite 状态库。
pub struct LocalDb {
    connection: Connection,
}

impl LocalDb {
    /// 打开数据库并确保表存在；路径为 `:memory:` 时使用内存数据库。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if path != Path::new(":memory:") {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("创建数据库目录 `{}` 失败: {error}", parent.display())
                    })?;
                }
            }
        }

        let connection = Connection::open(path)
            .map_err(|error| format!("打开数据库 `{}` 失败: {error}", path.display()))?;
        let db = Self { connection };
        db.init_schema()?;
        Ok(db)
    }

    /// 建立 installed_packages 与 installed_files 两张表。
    fn init_schema(&self) -> Result<(), String> {
        self.connection
            .execute_batch(
                "
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS installed_packages (
                    name         TEXT PRIMARY KEY NOT NULL,
                    version      TEXT NOT NULL,
                    installed_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                    origin       TEXT NOT NULL DEFAULT 'native'
                );

                CREATE TABLE IF NOT EXISTS installed_files (
                    package_name TEXT NOT NULL,
                    file_path    TEXT NOT NULL,
                    PRIMARY KEY (package_name, file_path),
                    FOREIGN KEY (package_name) REFERENCES installed_packages(name) ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS installed_hooks (
                    package_name TEXT NOT NULL,
                    phase        TEXT NOT NULL,
                    script       BLOB NOT NULL,
                    PRIMARY KEY (package_name, phase),
                    FOREIGN KEY (package_name) REFERENCES installed_packages(name) ON DELETE CASCADE
                );
                ",
            )
            .map_err(|error| format!("初始化数据库表失败: {error}"))?;

        self.ensure_origin_column()
    }

    /// 旧数据库升级：installed_packages 缺少 origin 时补列。
    fn ensure_origin_column(&self) -> Result<(), String> {
        let mut statement = self
            .connection
            .prepare("PRAGMA table_info(installed_packages)")
            .map_err(|error| format!("检查 installed_packages 表结构失败: {error}"))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| format!("读取 installed_packages 列失败: {error}"))?;
        let mut has_origin = false;
        for column in columns {
            if column.map_err(|error| format!("读取列名失败: {error}"))? == "origin" {
                has_origin = true;
                break;
            }
        }
        if !has_origin {
            self.connection
                .execute(
                    "ALTER TABLE installed_packages ADD COLUMN origin TEXT NOT NULL DEFAULT 'native'",
                    [],
                )
                .map_err(|error| format!("为 installed_packages 添加 origin 失败: {error}"))?;
        }
        Ok(())
    }

    /// 读取全部已安装包，按包名排序。
    pub fn list_installed(&self) -> Result<Vec<InstalledPackage>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT name, version, installed_at, origin
                 FROM installed_packages
                 ORDER BY name",
            )
            .map_err(|error| format!("准备查询已安装包失败: {error}"))?;

        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| format!("查询已安装包失败: {error}"))?;

        let mut installed = Vec::new();
        for row in rows {
            let (name, version_raw, installed_at, origin) =
                row.map_err(|error| format!("读取已安装包记录失败: {error}"))?;
            let version = Version::parse(&version_raw)
                .unwrap_or_else(|_| Version::parse("0").expect("0 必定是合法版本"));
            installed.push(InstalledPackage {
                name,
                version,
                version_raw,
                installed_at,
                origin,
            });
        }

        Ok(installed)
    }

    /// 读取某个包记录的文件清单，按路径排序。
    pub fn list_files(&self, package_name: &str) -> Result<Vec<String>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT file_path
                 FROM installed_files
                 WHERE package_name = ?1
                 ORDER BY file_path",
            )
            .map_err(|error| format!("准备查询文件清单失败: {error}"))?;

        let rows = statement
            .query_map(params![package_name], |row| row.get::<_, String>(0))
            .map_err(|error| format!("查询文件清单失败: {error}"))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("读取文件清单失败: {error}"))
    }

    /// 判断某个包是否已安装。
    pub fn package_exists(&self, package_name: &str) -> Result<bool, String> {
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM installed_packages WHERE name = ?1",
                params![package_name],
                |row| row.get(0),
            )
            .map_err(|error| format!("查询包 `{package_name}` 是否存在失败: {error}"))?;
        Ok(count > 0)
    }

    /// 判断某个文件路径是否仍被其他包引用。
    pub fn file_claimed_by_other(
        &self,
        package_name: &str,
        file_path: &str,
    ) -> Result<bool, String> {
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*)
                 FROM installed_files
                 WHERE file_path = ?1 AND package_name <> ?2",
                params![file_path, package_name],
                |row| row.get(0),
            )
            .map_err(|error| format!("查询文件 `{file_path}` 的其他属主失败: {error}"))?;
        Ok(count > 0)
    }

    /// 在一个事务里删除包的数据库记录和文件清单。
    ///
    /// 文件系统删除必须先成功；本函数只负责数据库侧。
    pub fn remove_package_records(&mut self, package_name: &str) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("开启卸载事务失败: {error}"))?;

        {
            let exists: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM installed_packages WHERE name = ?1",
                    params![package_name],
                    |row| row.get(0),
                )
                .map_err(|error| format!("查询待卸载包 `{package_name}` 失败: {error}"))?;
            if exists == 0 {
                return Err(format!("包 `{package_name}` 未安装"));
            }

            transaction
                .execute(
                    "DELETE FROM installed_files WHERE package_name = ?1",
                    params![package_name],
                )
                .map_err(|error| format!("删除包 `{package_name}` 的文件记录失败: {error}"))?;
            transaction
                .execute(
                    "DELETE FROM installed_hooks WHERE package_name = ?1",
                    params![package_name],
                )
                .map_err(|error| format!("删除包 `{package_name}` 的 hook 记录失败: {error}"))?;
            transaction
                .execute(
                    "DELETE FROM installed_packages WHERE name = ?1",
                    params![package_name],
                )
                .map_err(|error| format!("删除包 `{package_name}` 的记录失败: {error}"))?;
        }

        transaction
            .commit()
            .map_err(|error| format!("提交卸载事务失败: {error}"))
    }

    /// 读取某个包保存的生命周期脚本。
    pub fn list_hooks(&self, package_name: &str) -> Result<Vec<HookScript>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT phase, script
                 FROM installed_hooks
                 WHERE package_name = ?1
                 ORDER BY phase",
            )
            .map_err(|error| format!("准备查询 hook 失败: {error}"))?;
        let rows = statement
            .query_map(params![package_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| format!("查询 hook 失败: {error}"))?;
        let mut hooks = Vec::new();
        for row in rows {
            let (phase, script) = row.map_err(|error| format!("读取 hook 记录失败: {error}"))?;
            hooks.push(HookScript { phase, script });
        }
        Ok(hooks)
    }

    /// 在一个事务里写入包、替换旧文件清单并写入新文件清单。
    pub fn commit(&mut self, installs: &[PackageInstall]) -> Result<(), String> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| format!("开启数据库事务失败: {error}"))?;

        {
            let mut package_statement = transaction
                .prepare(
                    "INSERT INTO installed_packages (name, version, origin)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(name) DO UPDATE SET
                         version = excluded.version,
                         origin = excluded.origin,
                         installed_at = excluded.installed_at",
                )
                .map_err(|error| format!("准备写入已安装包失败: {error}"))?;

            let mut delete_files_statement = transaction
                .prepare("DELETE FROM installed_files WHERE package_name = ?1")
                .map_err(|error| format!("准备删除旧文件清单失败: {error}"))?;

            let mut delete_hooks_statement = transaction
                .prepare("DELETE FROM installed_hooks WHERE package_name = ?1")
                .map_err(|error| format!("准备删除旧 hook 失败: {error}"))?;

            let mut hook_statement = transaction
                .prepare(
                    "INSERT INTO installed_hooks (package_name, phase, script)
                     VALUES (?1, ?2, ?3)",
                )
                .map_err(|error| format!("准备写入 hook 失败: {error}"))?;

            let mut file_statement = transaction
                .prepare(
                    "INSERT INTO installed_files (package_name, file_path)
                     VALUES (?1, ?2)",
                )
                .map_err(|error| format!("准备写入文件清单失败: {error}"))?;

            for install in installs {
                package_statement
                    .execute(params![
                        &install.package.name,
                        &install.version_raw,
                        &install.origin
                    ])
                    .map_err(|error| format!("写入包 `{}` 失败: {error}", install.package.name))?;

                delete_files_statement
                    .execute(params![&install.package.name])
                    .map_err(|error| {
                        format!(
                            "清理包 `{}` 的旧文件清单失败: {error}",
                            install.package.name
                        )
                    })?;

                delete_hooks_statement
                    .execute(params![&install.package.name])
                    .map_err(|error| {
                        format!("清理包 `{}` 的旧 hook 失败: {error}", install.package.name)
                    })?;

                for hook in &install.hooks {
                    hook_statement
                        .execute(params![&install.package.name, &hook.phase, &hook.script])
                        .map_err(|error| {
                            format!(
                                "写入包 `{}` 的 hook `{}` 失败: {error}",
                                install.package.name, hook.phase
                            )
                        })?;
                }

                for file in &install.files {
                    file_statement
                        .execute(params![&install.package.name, file])
                        .map_err(|error| {
                            format!(
                                "写入包 `{}` 的文件 `{file}` 失败: {error}",
                                install.package.name
                            )
                        })?;
                }
            }
        }

        transaction
            .commit()
            .map_err(|error| format!("提交数据库事务失败: {error}"))
    }
}
