//! 版本号与 Capability 的解析、匹配和展示。
//!
//! Capability 表示“某个包名 + 可选版本约束”：
//! - 只有包名，如 `glibc`，表示任意版本都可以；
//! - 带约束，如 `libfoo >= 2.0`，表示只有满足该约束的版本才可以。

use std::fmt;

/// 点分十进制版本号，例如 `1.5`、`2.1`、`1.0.1`。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    segments: Vec<u64>,
}

impl Version {
    /// 解析点分十进制版本号；每一段必须是非负整数。
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("版本号不能为空".to_string());
        }

        let mut segments = Vec::new();
        for segment in input.split('.') {
            if segment.is_empty() {
                return Err(format!("版本号 `{input}` 含有空段"));
            }
            let value = segment
                .parse::<u64>()
                .map_err(|_| format!("版本号 `{input}` 的段 `{segment}` 不是非负整数"))?;
            segments.push(value);
        }

        Ok(Self { segments })
    }
}

impl fmt::Display for Version {
    /// 按原始点分结构输出版本号。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self
            .segments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(".");
        write!(formatter, "{text}")
    }
}

/// Capability 支持的三种比较操作符。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapabilityOp {
    GreaterThanOrEqual,
    LessThan,
    Equal,
}

impl CapabilityOp {
    /// 返回操作符的规范文本。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GreaterThanOrEqual => ">=",
            Self::LessThan => "<",
            Self::Equal => "=",
        }
    }

    /// 判断候选版本是否满足“候选 操作符 约束版本”的关系。
    fn matches(self, candidate: &Version, bound: &Version) -> bool {
        match self {
            Self::GreaterThanOrEqual => candidate >= bound,
            Self::LessThan => candidate < bound,
            Self::Equal => candidate == bound,
        }
    }
}

impl fmt::Display for CapabilityOp {
    /// 直接输出操作符文本。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一个包名加上可选的版本约束。
///
/// `parse` 保证 `op` 与 `version` 同时为 `Some` 或同时为 `None`。
/// 只有包名时，这两个字段都是 `None`，语义是“任意版本都满足”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub op: Option<CapabilityOp>,
    pub version: Option<Version>,
}

impl Capability {
    /// 解析 `包名` 或 `包名 操作符 版本`；非法格式返回中文错误。
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if input.is_empty() {
            return Err("能力字符串不能为空".to_string());
        }

        if let Some((name, op, version_text)) = split_operator(input) {
            let name = name.trim();
            let version_text = version_text.trim();

            if name.is_empty() {
                return Err(format!("能力 `{input}` 缺少包名"));
            }
            if name
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '<' | '>' | '='))
            {
                return Err(format!("能力 `{input}` 的包名不能包含空白或比较操作符"));
            }
            if version_text.is_empty() {
                return Err(format!("能力 `{input}` 缺少版本号"));
            }

            let version = Version::parse(version_text)
                .map_err(|error| format!("能力 `{input}` 的版本号无效: {error}"))?;

            return Ok(Self {
                name: name.to_string(),
                op: Some(op),
                version: Some(version),
            });
        }

        if input.contains('>') {
            return Err(format!(
                "能力 `{input}` 使用了不支持的比较操作符 `>`；本阶段仅支持 `>=`、`<`、`=`"
            ));
        }
        if input.chars().any(char::is_whitespace) {
            return Err(format!(
                "无法解析能力 `{input}`；应为 `包名` 或 `包名 >= 版本`（也支持 `<`、`=`）"
            ));
        }

        Ok(Self {
            name: input.to_string(),
            op: None,
            version: None,
        })
    }

    /// 判断某个具体版本是否满足本能力要求；无版本约束时恒为真。
    pub fn matches(&self, version: &Version) -> bool {
        match (self.op, self.version.as_ref()) {
            (None, None) => true,
            (Some(op), Some(bound)) => op.matches(version, bound),
            _ => false,
        }
    }
}

impl fmt::Display for Capability {
    /// 输出规范形式，例如 `libfoo >= 2.0` 或裸包名 `glibc`。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.op, self.version.as_ref()) {
            (Some(op), Some(version)) => write!(formatter, "{} {op} {version}", self.name),
            _ => formatter.write_str(&self.name),
        }
    }
}

/// 找到输入中最靠前的操作符，并切成“名字、操作符、版本文本”三段。
fn split_operator(input: &str) -> Option<(&str, CapabilityOp, &str)> {
    let candidates = [
        input
            .find(">=")
            .map(|index| (index, CapabilityOp::GreaterThanOrEqual, 2)),
        input
            .find('<')
            .map(|index| (index, CapabilityOp::LessThan, 1)),
        input.find('=').map(|index| (index, CapabilityOp::Equal, 1)),
    ];

    candidates
        .into_iter()
        .flatten()
        .min_by_key(|(index, _, _)| *index)
        .map(|(index, op, length)| (&input[..index], op, &input[index + length..]))
}
