use xrpkg::capability::{Capability, Version};
use xrpkg::resolvo_solver::{self, incremental_plan, resolve_install};
use xrpkg::solver::{Package, Solver};
use xrpkg::state::InstalledPackage;

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

fn chain() -> Vec<Package> {
    vec![
        package("libfoo", "1.5", &[]),
        package("libbar", "1.0", &["libfoo >= 1.0"]),
        package("app", "1.0", &["libbar >= 1.0"]),
    ]
}

fn multiple_versions() -> Vec<Package> {
    vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo >= 2.0"]),
    ]
}

fn unsatisfiable() -> Vec<Package> {
    vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo >= 3.0"]),
    ]
}

fn names(resolution: &xrpkg::solver::Resolution) -> Vec<&str> {
    resolution
        .packages()
        .iter()
        .map(|package| package.name.as_str())
        .collect()
}

#[test]
fn resolvo_chain_is_installed_dependencies_first() {
    let packages = chain();

    let resolution = resolvo_solver::resolve(&packages, "app").unwrap();

    assert_eq!(names(&resolution), ["libfoo", "libbar", "app"]);
    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn resolvo_selects_the_highest_matching_version() {
    let packages = multiple_versions();

    let resolution = resolvo_solver::resolve(&packages, "app").unwrap();

    assert_eq!(names(&resolution), ["libfoo", "app"]);
    assert_eq!(resolution.version_of("libfoo"), Some(&version("2.1")));
}

#[test]
fn resolvo_honors_equality_constraints() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("app", "1.0", &["libfoo = 1.5"]),
    ];

    let resolution = resolvo_solver::resolve(&packages, "app").unwrap();

    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn resolvo_reports_a_clear_conflict() {
    let packages = unsatisfiable();

    let error = resolvo_solver::resolve(&packages, "app").unwrap_err();

    assert_eq!(
        error,
        "无解: 包 `app` 依赖 `libfoo >= 3.0`，但仓库中没有满足该约束的版本；`libfoo` 可用版本: 1.5, 2.1"
    );
}

#[test]
fn resolvo_and_toy_solver_agree_on_solved_plans() {
    for packages in [chain(), multiple_versions()] {
        let toy = Solver::new(&packages).resolve("app").unwrap();
        let industrial = resolvo_solver::resolve(&packages, "app").unwrap();

        assert_eq!(toy.packages(), industrial.packages());
    }
}

#[test]
fn resolvo_reports_a_missing_root_package() {
    let error = resolvo_solver::resolve(&chain(), "missing-app").unwrap_err();

    assert!(error.contains("missing-app"), "{error}");
    assert!(error.contains("无解"), "{error}");
}

fn installed(name: &str, version_input: &str) -> InstalledPackage {
    InstalledPackage {
        name: name.to_string(),
        version: version(version_input),
        version_raw: version_input.to_string(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        origin: "native".to_string(),
    }
}

#[test]
fn installed_packages_are_roots_but_excluded_from_the_incremental_plan() {
    let packages = vec![
        package("app", "1.0", &[]),
        package("plugin", "1.0", &["app >= 1.0"]),
    ];
    let installed = vec![installed("app", "1.0")];

    let resolution = resolve_install(&packages, "plugin", &installed).unwrap();
    let changes = incremental_plan(&resolution, &installed);

    assert_eq!(resolution.version_of("app"), Some(&version("1.0")));
    assert_eq!(names(&resolution), ["app", "plugin"]);
    assert_eq!(
        changes
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>(),
        ["plugin"]
    );
}

#[test]
fn installed_version_is_favored_over_a_newer_candidate() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("newpkg", "1.0", &["libfoo >= 1.0"]),
    ];
    let installed = vec![installed("libfoo", "1.5")];

    let resolution = resolve_install(&packages, "newpkg", &installed).unwrap();

    assert_eq!(resolution.version_of("libfoo"), Some(&version("1.5")));
}

#[test]
fn installed_version_can_upgrade_when_a_new_requirement_needs_it() {
    let packages = vec![
        package("libfoo", "1.5", &[]),
        package("libfoo", "2.1", &[]),
        package("newpkg", "1.0", &["libfoo >= 2.0"]),
    ];
    let installed = vec![installed("libfoo", "1.5")];

    let resolution = resolve_install(&packages, "newpkg", &installed).unwrap();
    let changes = incremental_plan(&resolution, &installed);

    assert_eq!(resolution.version_of("libfoo"), Some(&version("2.1")));
    assert_eq!(
        changes
            .iter()
            .map(|package| package.name.as_str())
            .collect::<Vec<_>>(),
        ["libfoo", "newpkg"]
    );
}
