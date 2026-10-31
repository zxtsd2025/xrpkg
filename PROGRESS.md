# 项目进度

## 已完成的里程碑 (Completed)
- [x] 阶段 1-19：基础 CLI、SQLite 状态库、依赖映射、GPG 签名、原生 `.xpkg` 格式、ABI 守门员、原子升级/降级。
- [x] 阶段 20-26：LFS 核心包构建（GCC、Glibc、Linux 6.7.4），并成功部署到物理 NVMe 分区（/dev/nvme0n1）。
- [x] 阶段 27：内核排雷（强制内置 ext4/NVMe 驱动，修复 PARTUUID 启动问题）。
- [x] 阶段 28：成功引导进入自建的 XROS beta1.0 系统。并使用兼容轨道白嫖了 Arch 的 systemd、NetworkManager、util-linux。
- [x] 阶段 28.5：在 XROS 内部成功运行 xrpkg 二进制。

## 当前架构环境
- **宿主机**：VirtualBox 中的 Arch Linux（通过 VS Code Remote-SSH 连接）。
- **目标系统**：XROS beta1.0（运行在 NVMe 硬盘 `/dev/nvme0n1` 上，已完美启动并拥有 root 权限）。
- **核心工具**：`xrpkg` 已在 XROS 内可用，但原生的系统包还很少，主要依赖兼容轨道（缝合 Arch 包）。

## 当前任务 (In Progress)
**阶段 29：跨生态仓库同步器（xrpkg sync）**
- 目标：开发 `xrpkg sync` 命令，让它能直接解析 Pacman 的源（Arch Linux），拉取包并转换成 XROS 可用的本地仓库。
- 核心要求：
  1. 解析 `core.db` (tar.gz格式的元数据包)。
  2. 根据 `config/deps_map.toml` 进行依赖过滤。
  3. 自动下载 `.pkg.tar.zst`，通过 ABI 守门员校验，然后收入本地 `repo/` 目录并重新签名。
- 优先只实现 Arch (pacman)，暂不碰 apt 和 dnf。