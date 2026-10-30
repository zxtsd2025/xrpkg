use xrpkg::capability::{Capability, Version};
use xrpkg::solver::{Package, Solver};

fn version(input: &str) -> Version {
    Version::parse(input).unwrap()
}

fn package(name: &str, version_input: &str, requires: &[&str]) -> Package {
    let requires = requires
        .iter()
        .map(|capability| Capability::parse(capability).unwrap())
        .collect();
    Package::new(name, version(version_input), requires)
}

#[test]
fn chain_is_installed_dependencies_first() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libbar", "1.0", &["libfoo >= 1.0"]),
        package("app", "1.0", &["libbar >= 1.0"]),
    ];

    let resolution = Solver::new(&packages).resolve("app").unwrap();
    let order: Vec<_> = resolution
        .packages()
        .iter()
        .map(|package| package.name.as_str())
        .collect();

    assert_eq!(order, ["libfoo", "libbar", "app"]);
    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn highest_matching_version_is_selected() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo >= 2.0"]),
    ];

    let resolution = Solver::new(&packages).resolve("app").unwrap();
    let order: Vec<_> = resolution
        .packages()
        .iter()
        .map(|package| package.name.as_str())
        .collect();

    assert_eq!(order, ["libfoo", "app"]);
    assert_eq!(resolution.version_of("libfoo"), Some(&version("2.1")));
}

#[test]
fn upper_bound_selects_an_older_candidate() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo < 2.0"]),
    ];

    let resolution = Solver::new(&packages).resolve("app").unwrap();

    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn equality_selects_the_exact_version() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo = 1.5"]),
    ];

    let resolution = Solver::new(&packages).resolve("app").unwrap();

    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn bare_capability_accepts_the_highest_version() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo"]),
    ];

    let resolution = Solver::new(&packages).resolve("app").unwrap();

    assert_eq!(resolution.version_of("libfoo"), Some(&version("2.1")));
}

#[test]
fn unsatisfiable_requirement_names_the_requester_and_available_versions() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo >= 3.0"]),
    ];

    let error = Solver::new(&packages).resolve("app").unwrap_err();
    let message = error.to_string();

    assert!(message.contains("app"), "{message}");
    assert!(message.contains("libfoo"), "{message}");
    assert!(message.contains(">= 3.0"), "{message}");
    assert!(message.contains("2.1"), "{message}");
    assert!(message.contains("无解"), "{message}");
}

#[test]
fn mutually_exclusive_requirements_report_a_conflict() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo >= 2.0", "libfoo < 2.0"]),
    ];

    let error = Solver::new(&packages).resolve("app").unwrap_err();
    let message = error.to_string();

    assert!(message.contains("app"), "{message}");
    assert!(message.contains("libfoo"), "{message}");
    assert!(message.contains("无解"), "{message}");
}

#[test]
fn missing_package_is_reported() {
    let packages = vec![package("app", "1.0", &["libghost >= 1.0"])];

    let error = Solver::new(&packages).resolve("app").unwrap_err();
    let message = error.to_string();

    assert!(message.contains("libghost"), "{message}");
    assert!(message.contains("无解"), "{message}");
}
