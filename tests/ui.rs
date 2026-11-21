//! CLI 输出本地化与 ASCII 回退的端到端测试。

use std::process::{Command, Output};

/// 在干净的语言环境下运行 xrpkg；显式设置 TERM=dumb 以禁用颜色。
fn run(args: &[&str], language: &str, extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xrpkg"));
    for key in [
        "LC_ALL",
        "LC_MESSAGES",
        "LC_CTYPE",
        "LANG",
        "XRPKG_LANG",
        "XRPKG_ASCII",
        "NO_COLOR",
        "CLICOLOR",
    ] {
        command.env_remove(key);
    }
    command.env("LANG", language).env("TERM", "dumb");
    for (key, value) in extra {
        command.env(key, value);
    }
    command.args(args).output().unwrap()
}

/// 拉取 stdout 并断言没有 ANSI 控制序列。
fn stdout_without_ansi(output: &Output) -> String {
    assert!(output.status.success(), "命令失败: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stdout.contains('\x1b'), "非 TTY 不应输出 ANSI: {stdout}");
    stdout
}

#[test]
fn chinese_utf8_help_is_chinese_and_plain() {
    let stdout = stdout_without_ansi(&run(&["--help"], "zh_CN.UTF-8", &[]));
    assert!(stdout.contains("用法:"), "{stdout}");
    assert!(stdout.contains("XROS beta1.0 包管理器"), "{stdout}");
    assert!(!stdout.contains('\u{FFFD}'), "出现替换字符: {stdout}");
}

#[test]
fn english_locale_help_is_english() {
    let stdout = stdout_without_ansi(&run(&["--help"], "en_US.UTF-8", &[]));
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(!stdout.contains("用法:"), "{stdout}");
    assert!(stdout.is_ascii(), "英文帮助不应含非 ASCII: {stdout}");
}

#[test]
fn non_utf8_chinese_locale_falls_back_to_ascii() {
    let stdout = stdout_without_ansi(&run(&["--help"], "zh_CN.GB18030", &[]));
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(stdout.is_ascii(), "非 UTF-8 终端应回退 ASCII: {stdout}");
}

#[test]
fn non_utf8_error_output_stays_ascii() {
    let output = run(&["--bogus"], "zh_CN.GB18030", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_ascii(), "非 UTF-8 错误输出应保持 ASCII: {stderr}");
}

#[test]
fn explicit_language_override_wins_over_lang() {
    let stdout = stdout_without_ansi(&run(
        &["--help"],
        "en_US.UTF-8",
        &[("XRPKG_LANG", "zh_CN.UTF-8")],
    ));
    assert!(stdout.contains("用法:"), "{stdout}");
}

#[test]
fn ascii_override_forces_english() {
    let stdout = stdout_without_ansi(&run(&["--help"], "zh_CN.UTF-8", &[("XRPKG_ASCII", "1")]));
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(stdout.is_ascii(), "{stdout}");
}

#[test]
fn no_arguments_shows_help_instead_of_installing() {
    let stdout = stdout_without_ansi(&run(&[], "en_US.UTF-8", &[]));
    assert!(stdout.contains("Usage:"), "{stdout}");
}

#[test]
fn version_reports_package_and_distribution() {
    let stdout = stdout_without_ansi(&run(&["--version"], "en_US.UTF-8", &[]));
    assert!(stdout.contains("xrpkg 0.1.0"), "{stdout}");
    assert!(stdout.contains("XROS beta1.0"), "{stdout}");
}
