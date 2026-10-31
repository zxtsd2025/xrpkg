use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use xrpkg::world::{build_world, WorldOptions};

/// 写一个只安装普通文件的最小配方；`build` 用于制造成功或失败场景。
fn write_recipe(directory: &Path, name: &str, version: &str, build: &[&str]) {
    let source = directory.join(format!("{name}-src"));
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("payload"), format!("{name} {version}\n")).unwrap();

    let build_lines = build
        .iter()
        .map(|step| format!("  \"{step}\",\n"))
        .collect::<String>();
    let recipe = format!(
        "name = \"{name}\"\n\
         version = \"{version}\"\n\
         source = \"{name}-src\"\n\
         requires = []\n\
         build = [\n{build_lines}]\n\
         package = [\n\
           \"install -Dm644 payload \\\"$PKGDIR/usr/share/{name}/payload\\\"\",\n\
         ]\n"
    );
    fs::write(directory.join(format!("{name}.toml")), recipe).unwrap();
}

/// 写一个 `recipes = [...]` 形式的系统构建清单。
fn write_system_config(directory: &Path, recipes: &[&str]) -> PathBuf {
    let entries = recipes
        .iter()
        .map(|recipe| format!("  \"{recipe}\",\n"))
        .collect::<String>();
    let path = directory.join("system.toml");
    fs::write(&path, format!("recipes = [\n{entries}]\n")).unwrap();
    path
}

/// 一组测试用的输出仓库、Rootfs、数据库和日志路径。
struct Paths {
    repo: PathBuf,
    rootfs: PathBuf,
    db: PathBuf,
    log: PathBuf,
}

fn paths(temp: &TempDir) -> Paths {
    Paths {
        repo: temp.path().join("repo"),
        rootfs: temp.path().join("rootfs"),
        db: temp.path().join("state/local.db"),
        log: temp.path().join("logs/build-world.log"),
    }
}

fn options(log: PathBuf) -> WorldOptions {
    WorldOptions {
        sandbox: false,
        jobs: Some(2),
        no_hooks: true,
        log_path: log,
    }
}

#[test]
fn builds_in_order_then_resumes_from_existing_archives() {
    let temp = TempDir::new().unwrap();
    write_recipe(temp.path(), "alpha", "1.0", &[]);
    write_recipe(temp.path(), "beta", "2.0", &[]);
    let config = write_system_config(temp.path(), &["alpha.toml", "beta.toml"]);
    let paths = paths(&temp);

    let report = build_world(
        &config,
        &paths.repo,
        &paths.rootfs,
        &paths.db,
        &options(paths.log.clone()),
    )
    .unwrap();
    assert_eq!(report.built, 2);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.installed, 2);
    assert_eq!(report.already_installed, 0);

    assert!(paths.repo.join("alpha-1.0.xpkg").is_file());
    assert!(paths.repo.join("beta-2.0.xpkg").is_file());
    assert!(paths.rootfs.join("usr/share/alpha/payload").is_file());
    assert!(paths.rootfs.join("usr/share/beta/payload").is_file());
    let db = xrpkg::state::LocalDb::open(&paths.db).unwrap();
    let installed = db.list_installed().unwrap();
    assert_eq!(
        installed
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );

    // 第二次运行：两个归档都已存在，并且数据库已记录相同版本。
    // 把 alpha 的 build 改成必然失败，证明续传路径根本没有重新构建。
    write_recipe(temp.path(), "alpha", "1.0", &["exit 3"]);
    let report = build_world(
        &config,
        &paths.repo,
        &paths.rootfs,
        &paths.db,
        &options(paths.log.clone()),
    )
    .unwrap();
    assert_eq!(report.built, 0);
    assert_eq!(report.skipped, 2);
    assert_eq!(report.installed, 0);
    assert_eq!(report.already_installed, 2);

    let log = fs::read_to_string(&paths.log).unwrap();
    assert!(log.contains("跳过构建"), "{log}");
    assert!(log.contains("已安装相同版本"), "{log}");
    assert!(log.contains("build-world 完成"), "{log}");
}

#[test]
fn failure_stops_later_packages_and_is_logged() {
    let temp = TempDir::new().unwrap();
    write_recipe(temp.path(), "broken", "1.0", &["exit 3"]);
    write_recipe(temp.path(), "later", "1.0", &[]);
    let config = write_system_config(temp.path(), &["broken.toml", "later.toml"]);
    let paths = paths(&temp);

    let error = build_world(
        &config,
        &paths.repo,
        &paths.rootfs,
        &paths.db,
        &options(paths.log.clone()),
    )
    .unwrap_err();

    assert!(error.contains("broken 1.0"), "{error}");
    assert!(error.contains("退出码 3"), "{error}");
    assert!(!paths.repo.join("broken-1.0.xpkg").exists());
    assert!(
        !paths.repo.join("later-1.0.xpkg").exists(),
        "前置包失败后不得继续构建"
    );
    assert!(!paths.rootfs.join("usr/share/later/payload").exists());
    let log = fs::read_to_string(&paths.log).unwrap();
    assert!(log.contains("broken 1.0"), "{log}");
    assert!(log.contains("构建失败"), "{log}");
    assert!(log.contains("后续包未处理"), "{log}");
}

#[test]
fn rejects_an_empty_system_configuration() {
    let temp = TempDir::new().unwrap();
    let config = write_system_config(temp.path(), &[]);
    let paths = paths(&temp);

    let error = build_world(
        &config,
        &paths.repo,
        &paths.rootfs,
        &paths.db,
        &options(paths.log.clone()),
    )
    .unwrap_err();
    assert!(error.contains("recipes 不能为空"), "{error}");
}

#[test]
fn linux_recipe_forces_nvme_ext4_efi_and_devtmpfs_builtin() {
    let recipe_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes/linux.toml");
    let recipe = fs::read_to_string(recipe_path).unwrap();

    for option in [
        "BLK_DEV_NVME",
        "EXT4_FS",
        "EFI",
        "FB_EFI",
        "VFAT_FS",
        "DEVTMPFS",
        "DEVTMPFS_MOUNT",
    ] {
        assert!(
            recipe.contains(&format!("--enable {option}")),
            "linux.toml 缺少 --enable {option}"
        );
        assert!(
            recipe.contains(&format!("CONFIG_{option}")),
            "linux.toml 缺少 CONFIG_{option} 的构建期断言"
        );
    }
    assert!(recipe.contains("make olddefconfig"));
    assert!(recipe.contains("grep -qx"));
}
