//! 安装/升级成功后的目标系统缓存触发器：`ldconfig` 与 `fc-cache`。
//!
//! 触发器只在事务和数据库提交完成之后执行，因此失败不回滚：`ld.so.cache`
//! 与 fontconfig 缓存不在包文件清单里，事务无法恢复它们，而且重建是幂等的。
//! 工具缺失或执行失败只打印 `warning:`，不能把已经成功的安装变成失败。
//!
//! 工具必须来自目标 Rootfs，并在目标 Rootfs 内执行（复用 hooks 的 chroot
//! 沙箱），不能拿宿主工具去改一个可能使用不同 glibc 的系统。

use crate::hooks;
use crate::ui;
use crate::xpkg::Rootfs;

/// 一次事务需要执行的触发器集合；每个触发器最多执行一次。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TriggerPlan {
    pub ldconfig: bool,
    pub font_cache: bool,
}

impl TriggerPlan {
    /// 是否有任何触发器需要执行。
    pub fn any(&self) -> bool {
        self.ldconfig || self.font_cache
    }
}

/// 根据本次发布和升级删除的文件决定要跑哪些触发器。
pub fn plan(changed: &[String], removed: &[String]) -> TriggerPlan {
    let mut plan = TriggerPlan::default();
    for path in changed.iter().chain(removed.iter()) {
        if is_ldconfig_path(path) {
            plan.ldconfig = true;
        }
        if is_font_cache_path(path) {
            plan.font_cache = true;
        }
    }
    plan
}

/// 事务提交后调用；任何失败都只警告，不返回错误。
pub fn after_install(rootfs: &Rootfs, changed: &[String], removed: &[String], no_hooks: bool) {
    let plan = plan(changed, removed);
    if !plan.any() {
        return;
    }
    if no_hooks {
        ui::note(&ui::pick(
            "跳过安装后缓存触发器（--no-hooks）",
            "Skipping post-install cache triggers (--no-hooks)",
        ));
        return;
    }

    if plan.ldconfig {
        run_tool(rootfs, "ldconfig", "usr/sbin/ldconfig", "trigger-ldconfig");
    }
    if plan.font_cache {
        run_tool(rootfs, "fc-cache", "usr/bin/fc-cache", "trigger-fc-cache");
    }
}

/// 在目标 Rootfs 内执行一个缓存工具；失败只打印 `warning:`。
fn run_tool(rootfs: &Rootfs, name: &str, tool_relative: &str, label: &str) {
    let tool = rootfs.path().join(tool_relative);
    if !tool.is_file() {
        eprintln!(
            "warning: {}",
            ui::pick(
                format!("目标 {} 不存在，跳过 {name} 缓存重建", tool.display()),
                format!(
                    "target {} does not exist; skipping {name} cache rebuild",
                    tool.display()
                ),
            )
        );
        return;
    }

    let script = format!("exec chroot /target {tool_relative}\n");
    match hooks::run_script_in_sandbox(rootfs, label, script.as_bytes()) {
        Ok(output) if output.status.success() => {
            ui::ok(&ui::pick(
                format!("已重建 {name} 缓存"),
                format!("Rebuilt {name} cache"),
            ));
        }
        Ok(output) => {
            let code = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            eprintln!(
                "warning: {}",
                ui::pick(
                    format!("目标 {name} 退出码 {code}；安装已提交，缓存未重建"),
                    format!(
                        "target {name} exited with {code}; install is committed but the cache was not rebuilt"
                    ),
                )
            );
            for stream in [&output.stdout, &output.stderr] {
                if !stream.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(stream));
                }
            }
        }
        Err(error) => {
            eprintln!(
                "warning: {}",
                ui::pick(
                    format!("无法在目标 Rootfs 内执行 {}: {error}", tool.display()),
                    format!(
                        "failed to run {} inside the target rootfs: {error}",
                        tool.display()
                    ),
                )
            );
        }
    }
}

/// 规整数据库里的相对路径：去掉开头的 `/` 和 `./`。
fn normalize(path: &str) -> &str {
    let path = path.trim_start_matches('/');
    path.strip_prefix("./").unwrap_or(path)
}

/// 是否需要重建动态链接器缓存。
fn is_ldconfig_path(path: &str) -> bool {
    let path = normalize(path);
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".so") || name.contains(".so.") {
        return true;
    }
    if path.starts_with("etc/ld.so.conf") {
        return true;
    }
    ["usr/lib/", "lib/", "lib64/", "usr/lib64/", "usr/local/lib/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

/// 是否需要重建 fontconfig 缓存。
fn is_font_cache_path(path: &str) -> bool {
    let path = normalize(path);
    [
        "usr/share/fonts/",
        "usr/local/share/fonts/",
        "etc/fonts/",
        "usr/share/fontconfig/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    }

    #[test]
    fn library_paths_require_ldconfig() {
        for path in [
            "usr/lib/libz.so.1",
            "/usr/lib/libz.so.1",
            "lib/libc.so.6",
            "lib64/libfoo.so",
            "usr/lib64/libfoo.so",
            "usr/local/lib/libfoo.so",
            "usr/lib/modules/6.7.4/vmlinuz",
            "etc/ld.so.conf.d/xrpkg.conf",
            "opt/lib/libcustom.so",
        ] {
            let plan = plan(&changed(&[path]), &[]);
            assert!(plan.ldconfig, "应为 ldconfig 触发器命中: {path}");
        }
    }

    #[test]
    fn font_paths_require_fc_cache() {
        for path in [
            "usr/share/fonts/TTF/DejaVuSans.ttf",
            "usr/local/share/fonts/test.otf",
            "etc/fonts/conf.d/99-xrpkg.conf",
            "usr/share/fontconfig/conf.avail/50-user.conf",
        ] {
            let plan = plan(&changed(&[path]), &[]);
            assert!(plan.font_cache, "应为 fc-cache 触发器命中: {path}");
        }
    }

    #[test]
    fn unrelated_paths_trigger_nothing() {
        let plan = plan(
            &changed(&["usr/bin/hello", "usr/share/doc/hello/readme.txt"]),
            &["usr/share/licenses/old.txt".to_string()],
        );
        assert_eq!(plan, TriggerPlan::default());
    }

    #[test]
    fn removed_upgrade_files_also_count() {
        let plan = plan(&[], &["usr/lib/libold.so.1".to_string()]);
        assert!(plan.ldconfig);
        assert!(!plan.font_cache);
    }

    #[test]
    fn both_triggers_are_planned_once() {
        let plan = plan(
            &changed(&[
                "usr/lib/libfoo.so.1",
                "usr/lib/libbar.so.2",
                "usr/share/fonts/TTF/a.ttf",
            ]),
            &[],
        );
        assert_eq!(
            plan,
            TriggerPlan {
                ldconfig: true,
                font_cache: true
            }
        );
    }
}
