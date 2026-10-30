use std::path::Path;

use xrpkg::metadata::load_repository;
use xrpkg::xpkg::read_metadata;

#[test]
fn every_repository_package_has_a_matching_xpkg_archive() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repository = load_repository(project_root.join("packages.toml")).unwrap();

    for package in repository {
        let archive = project_root
            .join("repo")
            .join(format!("{}-{}.xpkg", package.name, package.version));
        assert!(archive.is_file(), "缺少包文件 {}", archive.display());

        let metadata = read_metadata(&archive).unwrap();
        assert_eq!(metadata.name, package.name);
        assert_eq!(metadata.version, package.version);
        assert_eq!(metadata.requires, package.requires);
    }
}
