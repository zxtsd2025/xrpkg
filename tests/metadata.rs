use std::path::Path;

use xrpkg::metadata::{load_repository, load_scenarios};

fn packages_toml() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("packages.toml")
}

#[test]
fn loads_all_three_scenarios() {
    let scenarios = load_scenarios(packages_toml()).unwrap();

    assert_eq!(scenarios.len(), 3);
    assert!(scenarios[0].title.contains("链式依赖"));
    assert!(scenarios[1].title.contains("多版本候选"));
    assert!(scenarios[2].title.contains("冲突无解"));
    assert!(scenarios.iter().all(|scenario| scenario.root == "app"));
}

#[test]
fn parses_requires_strings_into_capabilities() {
    let scenarios = load_scenarios(packages_toml()).unwrap();

    let libbar = scenarios[0]
        .packages
        .iter()
        .find(|package| package.name == "libbar")
        .unwrap();
    assert_eq!(libbar.requires.len(), 1);
    assert_eq!(libbar.requires[0].name, "libfoo");
    assert_eq!(libbar.requires[0].to_string(), "libfoo >= 1.0");

    let app = scenarios[0]
        .packages
        .iter()
        .find(|package| package.name == "app")
        .unwrap();
    assert_eq!(app.requires[0].to_string(), "libbar >= 1.0");
}

#[test]
fn loads_multiple_versions_and_unsatisfiable_constraint() {
    let scenarios = load_scenarios(packages_toml()).unwrap();

    let foo_versions = scenarios[1]
        .packages
        .iter()
        .filter(|package| package.name == "libfoo")
        .map(|package| package.version.to_string())
        .collect::<Vec<_>>();
    assert_eq!(foo_versions, ["1.5", "2.1"]);

    let conflicting_app = scenarios[2]
        .packages
        .iter()
        .find(|package| package.name == "app")
        .unwrap();
    assert_eq!(conflicting_app.requires[0].to_string(), "libfoo >= 3.0");
}

#[test]
fn reports_a_missing_config_file() {
    let missing = Path::new(env!("CARGO_MANIFEST_DIR")).join("does-not-exist.toml");

    assert!(load_scenarios(missing).is_err());
}

#[test]
fn loads_the_install_repository() {
    let repository = load_repository(packages_toml()).unwrap();

    assert_eq!(repository.len(), 6);
    let foo_versions = repository
        .iter()
        .filter(|package| package.name == "libfoo")
        .map(|package| package.version.to_string())
        .collect::<Vec<_>>();
    assert_eq!(foo_versions, ["1.5", "2.1"]);

    let app = repository
        .iter()
        .find(|package| package.name == "app")
        .unwrap();
    assert_eq!(app.requires[0].to_string(), "libbar >= 1.0");
}
