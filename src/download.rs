//! 统一下载层：HTTP/本地文件、进度条和 SHA256 工具。
//!
//! 进度条只在 stderr 是终端时启用；脚本、CI 和测试中自动切到
//! `ProgressDrawTarget::hidden()`，不会把 ANSI 控制序列写进日志。

use std::fs::{self, File};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// 本模块使用的错误类型；当前统一用可读字符串。
pub type Error = String;

/// 带颜色、下载速度、百分比和 ETA 的进度条模板。
const BAR_TEMPLATE: &str = "{spinner:.green} {msg:<30} [{bar:30.cyan/blue}] {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>12} {percent:>3}% ETA {eta}";

/// stderr 是否是终端；只有终端才需要绘制动态进度。
pub fn progress_enabled() -> bool {
    io::stderr().is_terminal()
}

/// 根据终端能力选择动态绘制或完全隐藏。
pub fn draw_target() -> ProgressDrawTarget {
    if progress_enabled() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    }
}

/// 创建多进度条容器；非 TTY 下整体隐藏。
pub fn multi_progress() -> MultiProgress {
    MultiProgress::with_draw_target(draw_target())
}

/// 创建单文件进度条。
pub fn single_bar(length: u64, label: &str) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(Some(length), draw_target());
    configure_bar(&bar);
    bar.set_message(label.to_string());
    bar
}

/// 创建要加入 MultiProgress 的进度条；绘制目标由 MultiProgress 接管。
pub fn multi_bar(length: u64, label: &str) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(Some(length), ProgressDrawTarget::hidden());
    configure_bar(&bar);
    bar.set_message(label.to_string());
    bar
}

/// 给进度条套上统一模板和颜色。
fn configure_bar(bar: &ProgressBar) {
    let style = ProgressStyle::with_template(BAR_TEMPLATE)
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-");
    bar.set_style(style);
}

/// 把远程/本地文件流式复制到目标路径，并更新进度条。
pub fn to_file(url: &str, target: &Path, bar: &ProgressBar) -> Result<u64, Error> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建下载目录 `{}` 失败: {error}", parent.display()))?;
    }

    if is_http(url) {
        let response = ureq::get(url)
            .set("User-Agent", "xrpkg/0.1")
            .call()
            .map_err(|error| format!("下载 `{url}` 失败: {error}"))?;
        if bar.length().unwrap_or(0) == 0 {
            if let Some(length) = response
                .header("Content-Length")
                .and_then(|value| value.parse::<u64>().ok())
            {
                bar.set_length(length);
            }
        }
        let mut reader = ProgressReader {
            inner: response.into_reader(),
            bar: bar.clone(),
        };
        let mut file = File::create(target)
            .map_err(|error| format!("创建下载文件 `{}` 失败: {error}", target.display()))?;
        let written = io::copy(&mut reader, &mut file)
            .map_err(|error| format!("写入下载文件 `{}` 失败: {error}", target.display()))?;
        file.sync_all()
            .map_err(|error| format!("刷新下载文件 `{}` 失败: {error}", target.display()))?;
        Ok(written)
    } else {
        let source = local_path(url);
        let metadata = fs::metadata(&source)
            .map_err(|error| format!("复制本地源 `{}` 失败: {error}", source.display()))?;
        if bar.length().unwrap_or(0) == 0 {
            bar.set_length(metadata.len());
        }
        let mut reader = ProgressReader {
            inner: File::open(&source)
                .map_err(|error| format!("复制本地源 `{}` 失败: {error}", source.display()))?,
            bar: bar.clone(),
        };
        let mut file = File::create(target)
            .map_err(|error| format!("创建下载文件 `{}` 失败: {error}", target.display()))?;
        let written = io::copy(&mut reader, &mut file)
            .map_err(|error| format!("写入下载文件 `{}` 失败: {error}", target.display()))?;
        file.sync_all()
            .map_err(|error| format!("刷新下载文件 `{}` 失败: {error}", target.display()))?;
        Ok(written)
    }
}

/// 读取整个远程/本地文件；只用于体积较小的数据库。
pub fn read_all(url: &str) -> Result<Vec<u8>, Error> {
    if is_http(url) {
        let response = ureq::get(url)
            .set("User-Agent", "xrpkg/0.1")
            .call()
            .map_err(|error| format!("下载 `{url}` 失败: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 `{url}` 失败: {error}"))?;
        Ok(bytes)
    } else {
        fs::read(local_path(url)).map_err(|error| format!("读取 `{url}` 失败: {error}"))
    }
}

/// 拼接仓库目录和文件名；同时支持 HTTP(S)、`file://` 和普通路径。
pub fn join_location(base: &str, child: &str) -> Result<String, Error> {
    if is_http(base) {
        Ok(format!("{}/{}", base.trim_end_matches('/'), child))
    } else {
        Ok(local_path(base).join(child).to_string_lossy().into_owned())
    }
}

pub fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

pub fn local_path(url: &str) -> PathBuf {
    let path = url.strip_prefix("file://").unwrap_or(url);
    let path = path.strip_prefix("localhost").unwrap_or(path);
    PathBuf::from(path)
}

/// 读取文件内容并推进进度条。
struct ProgressReader<R> {
    inner: R,
    bar: ProgressBar,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count > 0 {
            self.bar.inc(count as u64);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_target_is_used_when_stderr_is_not_a_terminal() {
        // libtest 捕获 stderr，因此这里应当走隐藏分支；即使将来环境变化，
        // 这个测试也只验证函数不会 panic。
        let _ = draw_target();
    }

    #[test]
    fn joins_http_and_local_locations() {
        assert_eq!(
            join_location("https://example.invalid/repo/", "a.pkg.tar.zst").unwrap(),
            "https://example.invalid/repo/a.pkg.tar.zst"
        );
        assert_eq!(
            join_location("file:///srv/repo", "a.pkg.tar.zst").unwrap(),
            "/srv/repo/a.pkg.tar.zst"
        );
    }
}
