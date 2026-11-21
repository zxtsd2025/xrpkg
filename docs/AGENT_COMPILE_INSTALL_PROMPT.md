# xrpkg 编译安装 · Agent 提示词

> 用法：把下面整段复制给新的 agent。默认在仓库所在 Arch/WSL 环境编译并安装 release 二进制；如果目标机是 XROS，请先向用户确认传输方式和安装路径，不要直接操作磁盘。

---

你接手的是 `xrpkg` 项目（阶段 32：跨生态仓库同步 + Arch/XFCE 兼容安装 + 并发下载/进度条）。你的任务是**编译、验证并安装**，不是重新设计或扩展功能。

## 环境与位置

- 仓库：`/home/zxtsd/xrpkg`（WSL Arch Linux 上的 ext4 路径；Windows 侧 UNC 为 `\\wsl.localhost\archlinux\home\zxtsd\xrpkg`，所有 cargo 命令应在 WSL 内执行）
- 工具链：`cargo` / `rustc` 1.100（若提示 “no default toolchain is configured”，在命令前加 `RUSTUP_TOOLCHAIN=stable`，或执行 `rustup default stable`）
- 构建依赖：C 工具链、`gpg`、`tar`、`gzip`、`zstd`、`unshare`
- 网络：首次构建可能需要访问 crates.io；Arch 同步测试/实跑还需要访问镜像源
- 目标架构：x86_64
- 磁盘：完整 `cargo test` 的 `target/debug` 可达十余 GB（本工作区实测约 18 GB）；release 二进制约 10 MiB。安装后可用 `rm -rf target/debug` 释放空间，但**不要未经用户同意执行 `cargo clean`**（它会删掉 release 产物）。

## 必读

1. `/home/zxtsd/xrpkg/AGENTS.md`
2. `/home/zxtsd/xrpkg/README.md`（中英双语；重点看 Requirements、Arch repository sync、Tests）
3. `/home/zxtsd/xrpkg/PROGRESS.md`

## 执行步骤

### 1. 进入仓库并确认基线

```sh
cd /home/zxtsd/xrpkg
git status --short
git rev-parse --short HEAD
cargo --version
rustc --version
```

记录当前是否有未提交改动。**不要执行 `git reset`、`git checkout .`、`git clean` 等会破坏用户工作的命令。**

### 2. 格式、静态检查和测试

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo test
```

`CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0` 可以显著缩小 `target/debug`，避免一次测试产生数 GB 调试产物；如果不需要控制磁盘，也可以直接运行 `cargo test`。

期望结果：

- `cargo fmt --check` 通过；
- `cargo clippy --all-targets -- -D warnings` 通过；
- `cargo test`：32 个测试目标、174 个测试通过、0 失败。

如果 `source_fetch` 的任一用例报 `Text file busy`（ETXTBSY），这是多个测试二进制并发共用 `~/.cache/xrpkg` 导致的既知环境抖动。先清理测试缓存再重跑：

```sh
rm -f ~/.cache/xrpkg/downloads/fetched-gzip.tar.gz \
      ~/.cache/xrpkg/downloads/fetched-xz.tar.xz
rm -rf ~/.cache/xrpkg/logs/*
cargo test
```

如果仍然失败，保留完整输出并报告，不要把它当成产品功能通过。

### 3. 编译 release

```sh
cargo build --release --locked
ls -lh target/release/xrpkg
target/release/xrpkg sync 2>&1 | head -n 3
```

最后一条命令应打印 `xrpkg sync` 的用法错误（因为缺少参数），用来确认二进制可执行；退出码非 0 是预期的。

### 4. 安装二进制

优先安装到当前用户 PATH，不改系统目录：

```sh
install -Dm755 target/release/xrpkg ~/.local/bin/xrpkg
command -v xrpkg
xrpkg sync 2>&1 | head -n 3
```

报告 `du -sh target`。如果用户抱怨磁盘占用，建议在确认已安装二进制后删除 `target/debug`；不要擅自执行 `cargo clean`。

如果用户明确要求系统级安装，再使用：

```sh
sudo install -Dm755 target/release/xrpkg /usr/local/bin/xrpkg
```

**不要擅自修改 `/etc`、`/dev/nvme*`、XROS 的启动分区或现有 Rootfs。**

### 5. 验证阶段 32 闭环（不访问网络）

这两个集成测试自带本地 mock 镜像和临时 GPG key，能验证同步、依赖过滤、ABI 拦截、Arch 包下载、软链接发布和 SQLite `origin='arch'`：

```sh
cargo test --test sync -- --nocapture
cargo test --test install_arch_repo -- --nocapture
```

可选的真实镜像冒烟测试（会下载大量 Arch core 包，先征得用户同意）：

```sh
xrpkg sync --all --jobs 4 --repo-dir /tmp/xrpkg-smoke
```

不要自动执行签名或安装真实系统包。`sync` 不会自动签名索引；生产 `xrpkg update`/`install` 仍需要用户提供 `KEY_ID` 并执行既有 `repo-sign` 流程。**不要生成、覆盖或删除用户的 GPG 密钥。**

### 6. 如果要部署到 XROS

先向用户确认目标机、传输方式和目标路径。默认建议：

```sh
# 在仓库构建机
scp target/release/xrpkg <user>@<xros-host>:/tmp/xrpkg

# 在 XROS 内
sudo install -Dm755 /tmp/xrpkg /usr/local/bin/xrpkg
xrpkg sync 2>&1 | head -n 3
```

在确认架构一致（x86_64）和 glibc 基线（最高 GLIBC_2.44）之前，不要把二进制直接塞进系统。不要碰 `/dev/nvme*` 或覆盖正在运行的系统文件。

## 约束

- 优先只编译/安装；发现编译或测试错误时，先定位并做**最小修复**，修完重新跑 fmt/clippy/test。
- 不要借机重构、改架构、引入新依赖或改变阶段 32 的范围。
- 不要执行破坏性命令，不要动用户的未提交文件。
- 不要擅自签名、安装真实软件包或修改系统启动配置。
- 所有临时验证都放在 `/tmp` 或测试自带的临时目录。
- 构建缓存可能达到数 GB；未经确认不要删除 `target/release` 或执行 `cargo clean`。

## 交付格式

请用中文报告：

1. 仓库路径、HEAD、`cargo/rustc` 版本；
2. `cargo fmt --check`、`cargo clippy`、`cargo test` 的实际结果（测试目标数、通过/失败数）；
3. `target/release/xrpkg` 的路径和大小；
4. 二进制安装位置，以及 `command -v xrpkg` 的结果；
5. 两个集成测试（`sync`、`install_arch_repo`）的结果；
6. 遇到的任何警告/失败、是否做了最小修复；
7. 如果没有执行真实镜像同步或 XROS 部署，明确说明原因和下一步需要用户确认什么。

---

## 可直接复制的短版

```text
你接手 /home/zxtsd/xrpkg（xrpkg 阶段 32，WSL Arch，Rust 1.100）。请先读 AGENTS.md、README.md、PROGRESS.md，然后完成编译、验证和安装：
1) cd /home/zxtsd/xrpkg
2) cargo fmt --check
3) cargo clippy --all-targets -- -D warnings
4) CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo test（期望 32 个测试目标、174 个测试通过；若 source_fetch 报 Text file busy，清理 ~/.cache/xrpkg/downloads/fetched-* 和 logs 后重跑）
5) cargo build --release --locked
6) install -Dm755 target/release/xrpkg ~/.local/bin/xrpkg
7) cargo test --test sync -- --nocapture
8) cargo test --test install_arch_repo -- --nocapture
不要改代码范围、不要生成/覆盖 GPG 密钥、不要签名或安装真实系统包、不要动 /dev/nvme* 或 XROS 启动分区。真实镜像同步和 XROS 部署必须先征得用户同意。构建后报告 du -sh target；target/debug 可达数 GB，安装 release 后可建议用户删除 target/debug，但不要擅自 cargo clean。最后报告 HEAD、工具链版本、三件套结果、二进制路径、安装位置和两个集成测试结果。
```