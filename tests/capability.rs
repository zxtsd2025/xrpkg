use xrpkg::capability::{Capability, CapabilityOp, Version};

fn version(input: &str) -> Version {
    Version::parse(input).unwrap()
}

fn capability(input: &str) -> Capability {
    Capability::parse(input).unwrap()
}

#[test]
fn parses_greater_than_or_equal() {
    let parsed = capability("libfoo >= 2.0");

    assert_eq!(parsed.name, "libfoo");
    assert_eq!(parsed.op, Some(CapabilityOp::GreaterThanOrEqual));
    assert_eq!(parsed.version, Some(version("2.0")));
}

#[test]
fn parses_less_than_and_equal() {
    let less = capability("libfoo < 2.0");
    assert_eq!(less.op, Some(CapabilityOp::LessThan));
    assert_eq!(less.version, Some(version("2.0")));

    let equal = capability("libfoo = 1.5");
    assert_eq!(equal.op, Some(CapabilityOp::Equal));
    assert_eq!(equal.version, Some(version("1.5")));
}

#[test]
fn parses_bare_capability_without_version_constraint() {
    let parsed = capability("glibc");

    assert_eq!(parsed.name, "glibc");
    assert_eq!(parsed.op, None);
    assert_eq!(parsed.version, None);
    assert!(parsed.matches(&version("2.44")));
    assert!(parsed.matches(&version("999.0")));
}

#[test]
fn accepts_optional_whitespace_around_operator() {
    let parsed = capability("  libfoo>=2.0  ");

    assert_eq!(parsed.name, "libfoo");
    assert_eq!(parsed.op, Some(CapabilityOp::GreaterThanOrEqual));
    assert_eq!(parsed.version, Some(version("2.0")));
}

#[test]
fn matches_each_supported_operator() {
    let greater = capability("libfoo >= 2.0");
    assert!(greater.matches(&version("2.0")));
    assert!(greater.matches(&version("2.1")));
    assert!(!greater.matches(&version("1.9")));

    let less = capability("libfoo < 2.0");
    assert!(less.matches(&version("1.9")));
    assert!(!less.matches(&version("2.0")));

    let equal = capability("libfoo = 2.0");
    assert!(equal.matches(&version("2.0")));
    assert!(!equal.matches(&version("2.1")));
}

#[test]
fn display_is_canonical() {
    assert_eq!(capability("libfoo>=2.0").to_string(), "libfoo >= 2.0");
    assert_eq!(capability("libfoo<1.5").to_string(), "libfoo < 1.5");
    assert_eq!(capability("libfoo=2.0").to_string(), "libfoo = 2.0");
    assert_eq!(capability("glibc").to_string(), "glibc");
}

#[test]
fn rejects_invalid_capability_strings() {
    for input in [
        "",
        "   ",
        "libfoo > 2.0",
        "libfoo>2.0",
        "libfoo>>=2.0",
        "libfoo >= ",
        ">= 2.0",
        "libfoo 2.0",
        "libfoo <= 2.0",
        "libfoo == 2.0",
        "libfoo >= two",
    ] {
        assert!(
            Capability::parse(input).is_err(),
            "本应拒绝非法输入: {input:?}"
        );
    }
}

#[test]
fn versions_compare_numerically_per_dot_segment() {
    assert!(version("1.10") > version("1.9"));
    assert!(version("2.0") > version("1.99"));
    assert!(version("1.0") < version("1.0.1"));
}
