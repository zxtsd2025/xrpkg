use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use xrpkg::capability::Version;
use xrpkg::solver::SelectedPackage;
use xrpkg::state::{LocalDb, PackageInstall};

fn selected(name: &str, version_input: &str) -> SelectedPackage {
    SelectedPackage {
        name: name.to_string(),
        version: Version::parse(version_input).unwrap(),
    }
}

fn install(name: &str, version_input: &str, files: &[&str]) -> PackageInstall {
    PackageInstall {
        package: selected(name, version_input),
        version_raw: version_input.to_string(),
        origin: "native".to_string(),
        files: files.iter().map(|file| file.to_string()).collect(),
        hooks: Vec::new(),
    }
}

#[test]
fn new_database_starts_empty() {
    let db = LocalDb::open(":memory:").unwrap();

    assert!(db.list_installed().unwrap().is_empty());
    assert!(db.list_files("app").unwrap().is_empty());
}

#[test]
fn commit_writes_packages_files_and_list_reads_them_sorted() {
    let mut db = LocalDb::open(":memory:").unwrap();

    db.commit(&[
        install("libfoo", "2.1", &["lib/libfoo.so"]),
        install("app", "1.0", &["share/app/readme.txt", "bin/app"]),
    ])
    .unwrap();

    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 2);
    assert_eq!(installed[0].name, "app");
    assert_eq!(installed[0].version, Version::parse("1.0").unwrap());
    assert!(!installed[0].installed_at.is_empty());
    assert_eq!(installed[1].name, "libfoo");
    assert_eq!(installed[1].version, Version::parse("2.1").unwrap());

    assert_eq!(
        db.list_files("app").unwrap(),
        ["bin/app", "share/app/readme.txt"]
    );
    assert_eq!(db.list_files("libfoo").unwrap(), ["lib/libfoo.so"]);
}

#[test]
fn committing_same_name_replaces_the_file_list() {
    let mut db = LocalDb::open(":memory:").unwrap();

    db.commit(&[install("libfoo", "1.5", &["lib/libfoo-old.so"])])
        .unwrap();
    db.commit(&[install("libfoo", "2.1", &["lib/libfoo-2.1.so"])])
        .unwrap();

    let installed = db.list_installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].version, Version::parse("2.1").unwrap());
    assert_eq!(db.list_files("libfoo").unwrap(), ["lib/libfoo-2.1.so"]);
}

fn temporary_db_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    env::temp_dir().join(format!("xrpkg-state-{}-{nonce}.db", std::process::id()))
}

#[test]
fn committed_packages_and_files_survive_reopening_the_database() {
    let path = temporary_db_path();

    {
        let mut db = LocalDb::open(&path).unwrap();
        db.commit(&[install("app", "1.0", &["bin/app"])]).unwrap();
    }

    {
        let db = LocalDb::open(&path).unwrap();
        let installed = db.list_installed().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "app");
        assert_eq!(installed[0].version, Version::parse("1.0").unwrap());
        assert_eq!(db.list_files("app").unwrap(), ["bin/app"]);
    }

    fs::remove_file(path).unwrap();
}
