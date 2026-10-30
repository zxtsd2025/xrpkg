//! xrpkg 的内部库：Capability、兼容轨道、签名仓库、LFS 隔离构建与并发日志、Rootfs 初始化、升级/降级事务、缓存清理、hooks 沙箱、元数据、resolvo、SQLite 状态、.xpkg、安装提交、卸载、.deb/.rpm/.pkg.tar.zst、依赖映射、ABI 守门和玩具求解器。

pub mod abi;
pub mod builder;
pub mod capability;
pub mod compat_arch;
pub mod compat_deb;
pub mod compat_rpm;
pub mod deps_map;
pub mod hooks;
pub mod installer;
pub mod metadata;
pub mod repo;
pub mod resolvo_solver;
pub mod sign;
pub mod solver;
pub mod state;
pub mod uninstall;
pub mod xpkg;
