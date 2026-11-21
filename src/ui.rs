//! 终端输出与本地化。
//!
//! 设计目标：
//! - 默认输出使用 ASCII 前缀（`::`、`->`、`[ok]`、`warning:`、`error:`），
//!   不使用 Unicode 框线、箭头或 Emoji；
//! - 只有在 UTF-8 终端且用户语言为中文时才输出中文，避免 GB18030/C
//!   终端出现满屏方块；
//! - 颜色只在对应输出流是 TTY 且未设置 `NO_COLOR` 时启用；
//! - 所有文案集中在本模块，避免各子命令各自拼输出。

use std::env;
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

/// 发行版名称；与 `XROS beta1.0` 品牌保持一致。
pub const DISTRO_NAME: &str = "XROS";
/// 发行版版本。
pub const DISTRO_VERSION: &str = "beta1.0";

/// 支持的界面语言。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Chinese,
}

impl Language {
    /// 根据 locale 字符串判断语言；不检查字符集。
    ///
    /// `zh*` 视为中文，其余（包括 `C`、`POSIX`、空值）视为英文。
    pub fn from_locale(locale: &str) -> Self {
        let normalized = locale.trim().to_ascii_lowercase();
        if normalized.starts_with("zh") {
            Self::Chinese
        } else {
            Self::English
        }
    }
}

/// 判断 locale 是否声明 UTF-8 字符集。
pub fn locale_is_utf8(locale: &str) -> bool {
    let normalized = locale.to_ascii_lowercase();
    normalized.contains("utf-8") || normalized.contains("utf8")
}

/// 运行时终端配置。
#[derive(Clone, Copy, Debug)]
pub struct Ui {
    language: Language,
    utf8: bool,
    color_stdout: bool,
    color_stderr: bool,
}

static UI: OnceLock<Ui> = OnceLock::new();

/// 返回进程级 UI 配置；环境变量在首次调用时读取一次。
pub fn ui() -> &'static Ui {
    UI.get_or_init(Ui::detect)
}

impl Ui {
    /// 构造一个明确的 UI 配置；主要用于测试。
    pub fn new(language: Language, utf8: bool, color_stdout: bool, color_stderr: bool) -> Self {
        Self {
            language,
            utf8,
            color_stdout,
            color_stderr,
        }
    }

    /// 从环境变量检测语言、字符集和颜色能力。
    pub fn detect() -> Self {
        let ascii = env_truthy("XRPKG_ASCII");
        let force = non_empty_env("XRPKG_LANG");
        let messages = first_non_empty(&["LC_ALL", "LC_MESSAGES", "LANG"]);
        let ctype = first_non_empty(&["LC_ALL", "LC_CTYPE", "LANG"]);

        let mut language = force
            .as_deref()
            .map(Language::from_locale)
            .unwrap_or_else(|| Language::from_locale(&messages));
        let mut utf8 = locale_is_utf8(&ctype);
        if let Some(force) = force.as_deref() {
            // 显式覆盖且写明了 UTF-8 时，即使 LC_CTYPE 仍是 C，也允许中文。
            utf8 |= locale_is_utf8(force);
        }
        if ascii || (language == Language::Chinese && !utf8) {
            language = Language::English;
            utf8 = false;
        }

        let color_allowed = !ascii
            && env::var_os("NO_COLOR").is_none()
            && env::var("CLICOLOR")
                .map(|value| value != "0")
                .unwrap_or(true)
            && env::var("TERM")
                .map(|value| value != "dumb")
                .unwrap_or(true);
        let color_stdout = color_allowed && io::stdout().is_terminal();
        let color_stderr = color_allowed && io::stderr().is_terminal();

        Self {
            language,
            utf8,
            color_stdout,
            color_stderr,
        }
    }

    /// 当前语言；中文只有在 UTF-8 环境下才会生效。
    pub fn language(&self) -> Language {
        if self.is_zh() {
            Language::Chinese
        } else {
            Language::English
        }
    }

    /// 是否应输出中文。
    pub fn is_zh(&self) -> bool {
        self.language == Language::Chinese && self.utf8
    }

    /// 选择静态文案。
    pub fn t(&self, en: &'static str, zh: &'static str) -> &'static str {
        if self.is_zh() {
            zh
        } else {
            en
        }
    }

    /// 选择运行时拼接的文案。
    pub fn pick(&self, zh: impl Into<String>, en: impl Into<String>) -> String {
        if self.is_zh() {
            zh.into()
        } else {
            en.into()
        }
    }

    /// 生成带数量的英文/中文短语，处理英文单复数。
    pub fn count(&self, n: usize, singular: &str, plural: &str, zh_unit: &str) -> String {
        if self.is_zh() {
            format!("{n} {zh_unit}")
        } else {
            format!("{n} {}", if n == 1 { singular } else { plural })
        }
    }

    /// 非 UTF-8 终端下把非 ASCII 字符替换为 `?`，避免中文诊断变成方块。
    fn safe(&self, text: &str) -> String {
        if self.utf8 {
            text.to_string()
        } else {
            text.chars()
                .map(|character| if character.is_ascii() { character } else { '?' })
                .collect()
        }
    }

    /// 输出高层次阶段标题，例如 `:: Installing packages`。
    pub fn phase(&self, text: &str) {
        let marker = self.paint_stdout("1;36", "::");
        println!("{marker} {}", self.safe(text));
    }

    /// 输出阶段内的一步操作，例如 `  -> Downloading ...`。
    pub fn step(&self, text: &str) {
        let marker = self.paint_stdout("36", "->");
        println!("  {marker} {}", self.safe(text));
    }

    /// 输出成功结果，例如 `  [ok] 3 packages installed`。
    pub fn ok(&self, text: &str) {
        let marker = self.paint_stdout("32", "[ok]");
        println!("  {marker} {}", self.safe(text));
    }

    /// 输出普通说明行。
    pub fn note(&self, text: &str) {
        println!("  {}", self.safe(text));
    }

    /// 输出警告到 stderr。
    pub fn warn(&self, text: &str) {
        let label = self.t("warning:", "警告:");
        let label = self.paint_stderr("1;33", label);
        eprintln!("{label} {}", self.safe(text));
    }

    /// 输出错误到 stderr。
    pub fn error(&self, text: &str) {
        let label = self.t("error:", "错误:");
        let label = self.paint_stderr("1;31", label);
        eprintln!("{label} {}", self.safe(text));
    }

    /// 输出空行；用于分隔事务摘要。
    pub fn blank(&self) {
        println!();
    }

    /// 打印三列表格，自动按显示宽度对齐；中文表头按双宽字符计算。
    pub fn table(&self, headers: [&str; 3], rows: &[[String; 3]]) {
        if rows.is_empty() {
            self.note(self.t("(none)", "(无)"));
            return;
        }
        let mut widths = [
            display_width(headers[0]),
            display_width(headers[1]),
            display_width(headers[2]),
        ];
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
        widths[0] = widths[0].max(12);
        widths[1] = widths[1].max(7);

        let header = format!(
            "  {}  {}  {}",
            pad(headers[0], widths[0]),
            pad(headers[1], widths[1]),
            pad(headers[2], widths[2])
        );
        println!("{}", self.paint_stdout("1", &self.safe(&header)));
        println!(
            "  {}  {}  {}",
            "-".repeat(widths[0]),
            "-".repeat(widths[1]),
            "-".repeat(widths[2])
        );
        for row in rows {
            println!(
                "  {}  {}  {}",
                self.safe(&pad(&row[0], widths[0])),
                self.safe(&pad(&row[1], widths[1])),
                self.safe(&pad(&row[2], widths[2]))
            );
        }
    }

    /// 打印两列表格；用于安装计划等不需要 origin 的场景。
    pub fn table2(&self, headers: [&str; 2], rows: &[[String; 2]]) {
        if rows.is_empty() {
            self.note(self.t("(none)", "(无)"));
            return;
        }
        let mut widths = [display_width(headers[0]), display_width(headers[1])];
        for row in rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(display_width(cell));
            }
        }
        widths[0] = widths[0].max(16);
        let header = format!(
            "  {}  {}",
            pad(headers[0], widths[0]),
            pad(headers[1], widths[1])
        );
        println!("{}", self.paint_stdout("1", &self.safe(&header)));
        println!("  {}  {}", "-".repeat(widths[0]), "-".repeat(widths[1]));
        for row in rows {
            println!("  {}  {}", pad(&row[0], widths[0]), pad(&row[1], widths[1]));
        }
    }

    /// 打印本地化帮助；`--help`、`-h` 和 `help` 共用。
    pub fn print_help(&self) {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        println!(
            "{} {} {}",
            DISTRO_NAME,
            DISTRO_VERSION,
            self.t("package manager", "包管理器")
        );
        println!();
        if self.is_zh() {
            self.help_zh();
        } else {
            self.help_en();
        }
    }

    /// 打印版本；`--version`、`-V` 和 `version` 共用。
    pub fn print_version(&self) {
        println!(
            "{} {} ({} {})",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            DISTRO_NAME,
            DISTRO_VERSION
        );
    }

    fn help_en(&self) {
        println!("Usage:");
        println!("  xrpkg <command> [options]");
        println!();
        println!("Commands:");
        println!("  install <package|file>   Install a package or local package file");
        println!("  remove <package>         Remove an installed package");
        println!("  update                   Refresh repository metadata");
        println!("  search <keyword>         Search repository indexes");
        println!("  list                     List installed packages");
        println!("  clean                    Remove stale cached packages/signatures");
        println!("  sync <mirror|url>        Synchronize an Arch mirror");
        println!("  sync --all               Synchronize all configured mirrors");
        println!("  build-world              Build and install the configured system");
        println!("  init                     Initialize a target root");
        println!("  repo-add <dir> <xpkg>    Add a package to a local repository");
        println!("  repo-sign <key> <file>   Create a detached signature");
        println!("  help                     Show this help");
        println!("  version                  Show version information");
        println!();
        println!("Options:");
        println!("  --root <dir>             Target root filesystem");
        println!("  --db <file>              SQLite package database");
        println!("  --config <file>          Repository configuration");
        println!("  --cache <dir>            Download cache");
        println!("  --mirrors <file>         Arch mirror configuration");
        println!("  --repo-dir <dir>         Local .xpkg repository directory");
        println!("  --jobs <n>               Parallel build/download jobs");
        println!("  --no-hooks               Skip lifecycle hooks");
        println!("  --no-sandbox             Disable build sandbox");
        println!("  --no-deps                Skip dependency preflight");
        println!("  --allow-downgrade        Allow version downgrade");
        println!("  --prefer-arch            Prefer Arch compatibility packages");
        println!("  --key <gpg-key>          Signing key for repo-add");
        println!("  --deb/--rpm/--arch <f>   Install a local compatibility package");
        println!("  --build <recipe>         Build and install one recipe");
        println!("  --remove <package>       Remove a package");
        println!("  -h, --help               Show this help");
        println!("  -V, --version            Show version");
        println!();
        println!("Environment:");
        println!("  XRPKG_LANG=zh_CN.UTF-8   Force Chinese or English UI language");
        println!("  XRPKG_ASCII=1            Force ASCII English output");
        println!("  NO_COLOR=1               Disable ANSI colors");
    }

    fn help_zh(&self) {
        println!("用法:");
        println!("  xrpkg <命令> [选项]");
        println!();
        println!("命令:");
        println!("  install <包名|文件>      安装仓库包或本地包文件");
        println!("  remove <包名>            卸载已安装包");
        println!("  update                   刷新仓库元数据");
        println!("  search <关键字>          搜索仓库索引");
        println!("  list                     列出已安装包");
        println!("  clean                    清理过期缓存和签名");
        println!("  sync <镜像|URL>          同步 Arch 镜像");
        println!("  sync --all               同步所有配置的镜像");
        println!("  build-world              构建并安装整套系统");
        println!("  init                     初始化目标 Rootfs");
        println!("  repo-add <目录> <xpkg>   把包加入本地仓库");
        println!("  repo-sign <密钥> <文件>  生成 detached 签名");
        println!("  help                     显示帮助");
        println!("  version                  显示版本");
        println!();
        println!("选项:");
        println!("  --root <目录>            目标 Rootfs");
        println!("  --db <文件>              SQLite 包数据库");
        println!("  --config <文件>          仓库配置");
        println!("  --cache <目录>           下载缓存");
        println!("  --mirrors <文件>         Arch 镜像配置");
        println!("  --repo-dir <目录>        本地 .xpkg 仓库目录");
        println!("  --jobs <n>               并行构建/下载数");
        println!("  --no-hooks               跳过生命周期 hooks");
        println!("  --no-sandbox             关闭构建沙箱");
        println!("  --no-deps                跳过依赖预检");
        println!("  --allow-downgrade        允许版本降级");
        println!("  --prefer-arch            优先 Arch 兼容包");
        println!("  --key <gpg-key>          repo-add 签名密钥");
        println!("  --deb/--rpm/--arch <f>   安装本地兼容包");
        println!("  --build <配方>           构建并安装单个配方");
        println!("  --remove <包名>          卸载包");
        println!("  -h, --help               显示帮助");
        println!("  -V, --version            显示版本");
        println!();
        println!("环境变量:");
        println!("  XRPKG_LANG=zh_CN.UTF-8   强制中文或英文界面");
        println!("  XRPKG_ASCII=1            强制 ASCII 英文输出");
        println!("  NO_COLOR=1               禁用 ANSI 颜色");
    }

    fn paint_stdout(&self, code: &str, text: &str) -> String {
        if self.color_stdout {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn paint_stderr(&self, code: &str, text: &str) -> String {
        if self.color_stderr {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// 读取第一个非空环境变量；用于 `LC_ALL > LC_MESSAGES > LANG`。
fn first_non_empty(keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = non_empty_env(key) {
            return value;
        }
    }
    String::new()
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_truthy(key: &str) -> bool {
    env::var(key)
        .map(|value| {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

/// 计算终端显示宽度；CJK 字符按两列计算。
fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn char_width(character: char) -> usize {
    let code = character as u32;
    if matches!(
        code,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    ) {
        2
    } else {
        1
    }
}

fn pad(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(padding))
}

/// 静态文案选择。
pub fn t(en: &'static str, zh: &'static str) -> &'static str {
    ui().t(en, zh)
}

/// 运行时文案选择。
pub fn pick(zh: impl Into<String>, en: impl Into<String>) -> String {
    ui().pick(zh, en)
}

/// 生成带数量的短语。
pub fn count(n: usize, singular: &str, plural: &str, zh_unit: &str) -> String {
    ui().count(n, singular, plural, zh_unit)
}

/// 是否中文界面。
pub fn is_zh() -> bool {
    ui().is_zh()
}

/// 输出阶段标题。
pub fn phase(text: &str) {
    ui().phase(text);
}

/// 输出阶段内步骤。
pub fn step(text: &str) {
    ui().step(text);
}

/// 输出成功行。
pub fn ok(text: &str) {
    ui().ok(text);
}

/// 输出说明行。
pub fn note(text: &str) {
    ui().note(text);
}

/// 输出警告。
pub fn warn(text: &str) {
    ui().warn(text);
}

/// 输出错误。
pub fn error(text: &str) {
    ui().error(text);
}

/// 输出未经格式化的原始行；非 UTF-8 终端下同样做 ASCII 安全替换。
pub fn raw(text: &str) {
    println!("{}", ui().safe(text));
}

/// 输出空行。
pub fn blank() {
    ui().blank();
}

/// 打印三列表格。
pub fn table(headers: [&str; 3], rows: &[[String; 3]]) {
    ui().table(headers, rows);
}

/// 打印两列表格。
pub fn table2(headers: [&str; 2], rows: &[[String; 2]]) {
    ui().table2(headers, rows);
}

/// 打印帮助。
pub fn print_help() {
    ui().print_help();
}

/// 打印版本。
pub fn print_version() {
    ui().print_version();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_utf8_is_detected() {
        assert_eq!(Language::from_locale("zh_CN.UTF-8"), Language::Chinese);
        assert!(locale_is_utf8("zh_CN.UTF-8"));
        assert!(locale_is_utf8("en_US.utf8"));
    }

    #[test]
    fn non_utf8_chinese_does_not_report_chinese() {
        let ui = Ui::new(Language::Chinese, false, false, false);
        assert!(!ui.is_zh());
        assert_eq!(ui.t("English", "中文"), "English");
    }

    #[test]
    fn c_locale_is_english() {
        assert_eq!(Language::from_locale("C"), Language::English);
        assert_eq!(Language::from_locale("POSIX"), Language::English);
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::English);
    }

    #[test]
    fn chinese_locale_uses_chinese_when_utf8() {
        let ui = Ui::new(Language::Chinese, true, false, false);
        assert!(ui.is_zh());
        assert_eq!(ui.t("English", "中文"), "中文");
        assert_eq!(ui.pick("中文", "English"), "中文");
    }

    #[test]
    fn english_counts_use_singular_and_plural() {
        let ui = Ui::new(Language::English, true, false, false);
        assert_eq!(ui.count(1, "file", "files", "个文件"), "1 file");
        assert_eq!(ui.count(2, "file", "files", "个文件"), "2 files");
    }

    #[test]
    fn chinese_counts_use_unit() {
        let ui = Ui::new(Language::Chinese, true, false, false);
        assert_eq!(ui.count(1, "file", "files", "个文件"), "1 个文件");
        assert_eq!(ui.count(2, "file", "files", "个文件"), "2 个文件");
    }

    #[test]
    fn non_utf8_output_is_ascii_safe() {
        let ui = Ui::new(Language::English, false, false, false);
        assert_eq!(ui.safe("错误"), "??");
        assert_eq!(ui.safe("error"), "error");
    }

    #[test]
    fn display_width_counts_cjk_as_two_columns() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(pad("中", 4), "中  ");
    }
}
