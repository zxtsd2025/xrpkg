# xrpkg

**English** · [中文](#zh)

<a id="english"></a>
## English

`xrpkg` is a package manager built for a from-scratch Linux distribution. It has two tracks:

- **Native track**: build LFS-style source recipes into `.xpkg` packages, then sign them for a repository.
- **Compatibility track**: install upstream `.deb`, `.rpm`, and `.pkg.tar.zst` packages while recording their files and hooks in one local SQLite database.

The project targets **glibc 2.44** on x86_64. The recipes currently checked in are the LFS 12.1 set (glibc 2.39 / Linux 6.7.4); see [Recipes](#recipes) and `PROGRESS.md` → “已知差距”.

> **Status:** experimental, phase 32. Native/compat install, Arch sync, concurrent downloads, repository signing, build flows, and the phase-32 compatibility publishing changes are covered by tests, but this is not yet a production package manager; a full XFCE/Xorg install on XROS is not yet documented.

### Highlights

- **`.xpkg` native format**  
  zstd-compressed tar archives with `metadata.toml`, file payload, and lifecycle hooks.

- **resolvo-based dependency solving**  
  Capability/version solving with incremental plans and installed-package tracking.

- **SQLite state**  
  `installed_packages`, `installed_files`, and `installed_hooks` are the source of truth for uninstall and upgrade.

- **ABI guard**  
  Every ELF file is checked against the GLIBC baseline before it is published to the target root.

- **Lifecycle hooks**  
  `pre_install`, `post_install`, `pre_remove`, `post_remove`, `pre_upgrade`, and `post_upgrade` run inside an unprivileged chroot sandbox.

- **Metadata fidelity**  
  SUID/SGID bits, ownership, and extended attributes (including `security.capability`) survive extraction, native packaging, and publication. Both GNU tar `SCHILY.xattr.*` and libarchive `LIBARCHIVE.xattr.*` PAX records are decoded.

- **Atomic transactions and crash recovery**  
  Every published file is written to a temporary file, fully synced, and moved into place with `rename(2)`. Old files are snapshotted under `<root>/.xrpkg-txn`; failures restore them, and a journal plus database-state check recovers after power loss.

- **Upgrade, downgrade, and uninstall rollback**  
  Old files are snapshotted, obsolete files are removed transactionally, and failed hooks or database commits restore both the filesystem and the previous package record.

- **OpenPGP repository signing**  
  Indexes and packages use detached `.sig` files verified against an explicit trusted keyring.

- **Isolated build sandbox**  
  Build and package steps run in a user/mount namespace with a read-only `/usr`, private `/tmp`, and bound source/output directories.

- **Arch repository sync and compatibility install**  
  Parse a Pacman `core.db`, download `.pkg.tar.zst` files concurrently, run the ABI guard, and install `origin = "arch"` packages through the compatibility track. External upgrades and uninstalls use the same transactional snapshots as native packages.

- **Publishing over occupied paths**  
  Files and links can be published over existing symlinks or hardlinks without writing through them. A symlink target may be created later by the same package as long as its nearest existing ancestor stays inside the root. `--no-deps` skips dependency preflight for closures resolved outside xrpkg.

- **Local `.xpkg` install**  
  `xrpkg install ./foo.xpkg` validates metadata, preflights dependencies, runs the ABI guard, and commits through the same transaction path as repository installs.

- **Standardized, localized CLI output**  
  Chinese/English is selected from `LC_ALL`/`LC_MESSAGES`/`LANG`; non-UTF-8 terminals automatically fall back to ASCII English. Actions use `::`, `->`, `[ok]`, `warning:`, and `error:`, with aligned tables; colors only appear on a TTY. `--help`, `--version`, and `list` are first-class commands.

- **Concurrent downloads with colored progress bars**  
  Per-package bars show speed, bytes, percentage, and ETA; they hide automatically when stderr is not a TTY.

- **LFS recipes**  
  Reference recipes for `hello`, `zlib`, `binutils`, `glibc`, `gcc`, `linux`, `bash`, and `coreutils`.

### Requirements

- Linux with user namespaces and `unshare`
- Rust 2021 toolchain (tested with 1.100). If `cargo` reports “no default toolchain is configured”, run `rustup default stable` or prefix commands with `RUSTUP_TOOLCHAIN=stable`.
- C toolchain (`cc`, `make`) for bundled SQLite/zstd/xz
- `gpg` for repository signatures
- `tar`, `gzip`, and `zstd` for compatibility and sync workflows
- `sudo` only for optional chroot/device-node experiments

Build and verify:

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Install the binary somewhere on `PATH`:

```sh
install -Dm755 target/release/xrpkg ~/.local/bin/xrpkg
# or system-wide
sudo install -Dm755 target/release/xrpkg /usr/local/bin/xrpkg
```

After configuring `config/mirrors.toml`, the release binary can be smoke-tested with:

```sh
xrpkg sync --all --jobs 4
```

For a copy-paste agent handoff, see [docs/AGENT_COMPILE_INSTALL_PROMPT.md](docs/AGENT_COMPILE_INSTALL_PROMPT.md).

### Disk usage

`cargo test` compiles debug dependencies and test binaries under `target/debug`; a full run can use several GB (debug info and incremental artifacts). For reference, a full debug test build in this workspace used about **6.9 GiB**, while `target/release` was about **444 MiB**. The installed `xrpkg` binary itself is only **9.7 MiB**.

After installing the release binary, reclaim space with:

```sh
# keep target/release, remove debug/test artifacts
rm -rf target/debug

# or remove all Cargo artifacts (also deletes target/release/xrpkg)
cargo clean
```

For disk-constrained CI, disable debug info and incremental compilation:

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo test
```

### Quick start

Build and install a native package:

```sh
xrpkg --build recipes/hello.toml \
  --root /tmp/xrpkg-root \
  --db /tmp/xrpkg-root/local.db
```

Install a local `.xpkg` directly:

```sh
xrpkg install ./hello-1.0.xpkg   --root /tmp/xrpkg-root   --db /tmp/xrpkg-root/local.db
```

The default target root is `/var/tmp/xrpkg-rootfs`. When run from the build tree, the default database is `var/lib/xrpkg/local.db`; an installed binary such as `/usr/bin/xrpkg` uses `/var/lib/xrpkg/local.db` instead.

Use `xrpkg list` to inspect installed packages; `xrpkg --help` shows all commands. State-changing operations print a compact transaction summary instead of dumping the whole database.

Remove a package:

```sh
xrpkg --remove hello --root /tmp/xrpkg-root --db /tmp/xrpkg-root/local.db
```

### Build behavior

Build and package commands run in a sandbox by default:

```text
/var/tmp/xrpkg-build
/var/tmp/xrpkg-build-sandbox
```

The sandbox:

- read-only binds `/usr` for the host toolchain;
- mounts a private tmpfs at `/tmp`;
- binds `/proc` and `/dev`;
- binds the prepared source tree at `/src` and the package output at `/pkg`;
- clears host environment variables;
- injects only `PATH`, `PKGDIR`, `SRCDIR`, `HOME=/tmp`, `TMPDIR=/tmp`, and `MAKEFLAGS`.

Default concurrency:

```text
MAKEFLAGS=-j8
```

Override it:

```sh
xrpkg --build recipes/linux.toml --jobs 4
```

A recipe can also provide:

```toml
[env]
MAKEFLAGS = "-j6"
```

CLI `--jobs` wins over `[env]`, and both win over the default.

Build logs are written outside the project:

```text
~/.cache/xrpkg/logs/<name>-build.log
~/.cache/xrpkg/logs/<name>-package.log
```

For debugging only:

```sh
xrpkg --build recipes/hello.toml --no-sandbox
```

### Source cache and temporary paths

Downloads follow the XDG cache specification:

```text
$XDG_CACHE_HOME/xrpkg/downloads
# or
~/.cache/xrpkg/downloads
```

Build temp directories live under `/var/tmp`:

```text
/var/tmp/xrpkg-build
/var/tmp/xrpkg-build-sandbox
/var/tmp/xrpkg-hooks
/var/tmp/xrpkg-upgrade
```

They can be redirected with:

```sh
XRPKG_BUILD_TMP=/tmp xrpkg --build recipes/hello.toml
```

### Repositories and OpenPGP

Repository configuration example:

```toml
keyring = "/etc/xrpkg/trusted.gpg"

[[repos]]
name = "local"
url = "file:///srv/xrpkg/repo"
```

The keyring contains exported public keys:

```sh
gpg --export <KEY_ID> > /etc/xrpkg/trusted.gpg
```

Sign a package and index:

```sh
xrpkg repo-add /srv/xrpkg/repo hello-1.0.xpkg --key <KEY_ID>
xrpkg repo-sign <KEY_ID> /srv/xrpkg/repo/xrpkg-repo.toml
```

Fetch and verify repository data:

```sh
xrpkg update --config ~/.config/xrpkg/repos.toml
xrpkg install hello --config ~/.config/xrpkg/repos.toml
```

`update` verifies `xrpkg-repo.toml.sig`; `install` verifies `<package>.xpkg.sig` before unpacking. A missing signature, a bad signature, or a key outside the configured keyring is rejected.

### Compatibility packages

```sh
xrpkg install ./package.deb
xrpkg install ./package.rpm
xrpkg install ./package.pkg.tar.zst
```

Debian, RPM, and Arch metadata are parsed, dependencies are mapped through `config/deps_map.toml`, and the resulting files and hooks are recorded in the native SQLite database. External packages are not converted into `.xpkg`. The mapping file covers both Debian/RPM dependency names and Arch sonames such as `libz.so` and `libsystemd.so`.

Use `--no-deps` when the dependency closure is resolved outside xrpkg (for example a hand-assembled Arch closure): it skips only the preflight check, while the ABI guard and transactional publish still run.

### Arch repository sync and installation

Configure one or more Arch repository directories in `config/mirrors.toml`:

```toml
[[mirrors]]
name = "tuna"
url = "https://mirrors.tuna.tsinghua.edu.cn/archlinux/core/os/x86_64/"
priority = 1
```

Sync all configured mirrors, a named mirror, or a direct URL:

```sh
xrpkg sync --all
xrpkg sync tuna
xrpkg sync https://mirrors.ustc.edu.cn/archlinux/core/os/x86_64/
```

`sync` downloads `.pkg.tar.zst` files concurrently, checks SHA256, runs the ABI guard, and indexes accepted packages with `origin = "arch"`. `--jobs <n>` controls the download worker count. Progress bars are colored and only drawn when stderr is a terminal; in scripts and CI they are hidden automatically.

A synced Arch package is installed through the compatibility track:

```sh
xrpkg install acl
```

Native `.xpkg` candidates win when both tracks provide the same name. Use `--prefer-arch` to force the Arch package. `xrpkg install` downloads the `.pkg.tar.zst` from the configured mirrors, runs the ABI guard, publishes files and symlinks, and records `origin = "arch"` in SQLite.

`--jobs <n>` also controls sync concurrency; the default is `min(available_parallelism, 8)`. `--repo-dir <dir>` overrides the default `repo/` destination. `sync` refreshes the local repository only; the existing signature policy is unchanged. Sign the index before `xrpkg update`:

```sh
xrpkg repo-sign <KEY_ID> repo/xrpkg-repo.toml
xrpkg update --config ~/.config/xrpkg/repos.toml
xrpkg install acl
```

### Upgrade and downgrade

Install or upgrade to the newest repository version:

```sh
xrpkg install pkg-a
```

Pin a version:

```sh
xrpkg install pkg-a=1.0
```

Allow an explicit downgrade:

```sh
xrpkg install pkg-a=1.0 --allow-downgrade
```

Without `--allow-downgrade`, a lower target version is rejected before download.

### Cache cleanup

```sh
xrpkg clean
```

`clean` keeps:

- the version currently installed in the database;
- the newest version in the current repository index.

It deletes other cached `.xpkg` files and their `.sig` files, then reports package count, signature count, and freed bytes.

### Rootfs initialization and chroot

Create the basic files for a target root:

```sh
xrpkg init --root /tmp/xrpkg-root
```

This creates `/etc/passwd`, `/etc/group`, `/etc/hosts`, `/etc/resolv.conf`, `/etc/nsswitch.conf`, `/etc/ld.so.conf`, and the standard `/bin`, `/sbin`, `/lib`, and `/lib64` symlinks.

For a chroot smoke test, install a working glibc and runtime libraries first, then create device nodes:

```sh
sudo mknod -m 666 /tmp/xrpkg-root/dev/null c 1 3
sudo mknod -m 600 /tmp/xrpkg-root/dev/console c 5 1
sudo chroot /tmp/xrpkg-root /bin/bash
```

### Project layout

```text
src/
  main.rs        CLI and command dispatch
  abi.rs         ELF GNU version check against the GLIBC baseline
  capability.rs  version parsing and package capability constraints
  builder.rs     source recipes, sandbox, logging
  installer.rs   staging, ABI checks, upgrade transactions
  fs_meta.rs     permissions, ownership, xattrs, and PAX decoding
  transaction.rs atomic publish, snapshots, journal, crash recovery
  uninstall.rs   database-driven removal and empty-directory cleanup
  world.rs       recipes/system.toml orchestration (build-world)
  hooks.rs       lifecycle hooks and hook sandbox
  repo.rs        repository indexes, cache, downloads, cleanup
  download.rs    streaming HTTP/local downloads and progress bars
  ui.rs          localized CLI output, tables, help, and color policy
  mirrors.rs     config/mirrors.toml loading and priority resolution
  sync.rs        Arch core.db sync and parallel ingest
  sign.rs        OpenPGP detached signatures
  state.rs       SQLite state
  metadata.rs    scenario/repository TOML loading
  deps_map.rs    external dependency and soname mapping
  solver.rs      toy solver kept as the resolvo baseline
  resolvo_solver.rs dependency solving
  xpkg.rs        .xpkg writer/reader and safe extraction
  compat_*.rs    deb/rpm/arch compatibility tracks
recipes/         LFS build recipes
config/          repository config, deps map, and mirrors.toml
tests/           integration tests
repo/            built package artifacts (ignored)
var/             local SQLite state (ignored)
```

### Recipes

| Recipe | Version |
|---|---|
| `hello` | 1.0 |
| `zlib` | 1.3.1 |
| `binutils` | 2.42 |
| `glibc` | 2.39 |
| `gcc` | 13.2.0 |
| `linux` | 6.7.4 |
| `bash` | 5.2.21 |
| `coreutils` | 9.4 |

> The ABI guard baseline is `GLIBC_2.44`, but the committed glibc recipe builds 2.39. `repo/glibc-2.44.xpkg` was produced outside this recipe set and cannot currently be rebuilt from this tree; see `PROGRESS.md` → “已知差距”.

### Tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Current local result: **174 tests passed across 32 test targets**. Some tests require `unshare`, user namespaces, `gpg`, `setcap` (root-only capability test), and `zstd`.

Test binaries share `~/.cache/xrpkg`, so a parallel run can occasionally fail with `Text file busy` (ETXTBSY). Remove `~/.cache/xrpkg/downloads/fetched-*` and rerun; making each test use a private cache directory is a known follow-up.

### Security model and scope

`xrpkg` validates:

- repository index signatures;
- package signatures;
- package SHA-256;
- ELF GLIBC symbol versions;
- archive paths and symlink targets;
- database file ownership before deleting shared files.

The build sandbox isolates mounts and the filesystem view, but it is not a virtual machine and does not create a separate network namespace. Do not treat untrusted recipes as safe.

### Limitations

- Not production-ready.
- The ABI baseline is `GLIBC_2.44`, but the committed glibc recipe builds 2.39; `repo/glibc-2.44.xpkg` cannot currently be rebuilt from this tree.
- LFS glibc/gcc recipes are present and configure-verified; full toolchain builds are long and hardware-dependent.
- Compat tracks record external packages in the native database, but do not rewrite their dependency metadata.
- `sync` does not sign the repository automatically; the existing `repo-sign` step is still required before `xrpkg update`.
- Only the Arch/Pacman ecosystem is synced in this phase; apt/dnf are not implemented.
- Repository signatures depend on a system `gpg` installation.
- The checked-in `repo/xrpkg-repo.toml` indexes only part of `repo/`; regenerate it with `repo-add` before treating it as a complete index.
- Tests share `~/.cache/xrpkg` and can collide under parallel execution (see Tests).

[中文](#zh)

---

<a id="zh"></a>
## 中文

[English](#english) · **中文**

`xrpkg` 是面向从零构建的 Linux 发行版的包管理器，包含两条轨道：

- **原生轨道**：把 LFS 风格的源码配方构建为 `.xpkg` 包，再为仓库签名。
- **兼容轨道**：安装上游 `.deb`、`.rpm`、`.pkg.tar.zst`，并把文件与 hooks 统一记录到本地 SQLite 数据库。

当前目标基线为 **glibc 2.44**、x86_64。仓库内已提交的配方仍是 LFS 12.1 那一套（glibc 2.39 / Linux 6.7.4）；见“配方”和 `PROGRESS.md` 的“已知差距”。

> **状态：** 实验性，阶段 32。原生/兼容安装、Arch 同步、并发下载、仓库签名、构建流程和阶段 32 的兼容发布改动都有测试覆盖，但还不是生产级包管理器；XROS 上的完整 XFCE/Xorg 安装尚未形成文档。

### 主要特性

- **原生 `.xpkg` 格式**：zstd 压缩 tar，包含 `metadata.toml`、文件载荷和生命周期 hooks。
- **resolvo 依赖求解**：支持能力/版本约束、增量计划和已安装包状态。
- **SQLite 状态**：`installed_packages`、`installed_files`、`installed_hooks` 是卸载和升级的唯一事实来源。
- **ABI 守门**：发布到目标 Rootfs 前检查 ELF 的 GLIBC 符号版本。
- **生命周期 hooks**：`pre_install`、`post_install`、`pre_remove`、`post_remove`、`pre_upgrade`、`post_upgrade` 在非特权 chroot 沙箱中执行。
- **元数据保真**：解包、原生打包和发布都会保留 SUID/SGID、属主和扩展属性（包括 `security.capability`）；同时解析 GNU tar `SCHILY.xattr.*` 与 libarchive `LIBARCHIVE.xattr.*` 两种 PAX 记录。
- **原子事务与崩溃恢复**：每个待发布文件先写入临时文件并 `fsync`，再用 `rename(2)` 原子替换。旧文件快照保存在 `<root>/.xrpkg-txn`；失败时恢复，断电后由日志加数据库状态自动判定提交或回滚。
- **升级、降级与卸载回滚**：旧文件进入快照，废弃文件在事务内清理；hook 或数据库提交失败时，文件系统和旧包记录一起恢复。
- **OpenPGP 仓库签名**：索引和包使用分离 `.sig`，按显式信任 keyring 验证。
- **隔离构建沙箱**：user/mount namespace、只读 `/usr`、独立 `/tmp`、绑定源码和输出目录。
- **Arch 仓库同步与兼容安装**：解析 Pacman `core.db`，并发下载 `.pkg.tar.zst`，通过 ABI 守门后按 `origin = "arch"` 走兼容轨道安装。外部包的升级与卸载同样使用事务快照。
- **覆盖已有路径的发布**：文件和链接可以覆盖已有的符号链接/硬链接，但绝不通过链接写穿到目标；符号链接可以先指向同批后续文件才会创建的路径，只要最近的已存在祖先仍在 Rootfs 内。`--no-deps` 用于依赖闭包已在 xrpkg 之外解决的场景。
- **本地 `.xpkg` 安装**：`xrpkg install ./foo.xpkg` 会校验元数据、预检依赖、运行 ABI 守门，并走与仓库安装相同的提交事务。
- **标准化、多语言 CLI**：根据 `LC_ALL`/`LC_MESSAGES`/`LANG` 选择中文或英文；非 UTF-8 终端自动回退到 ASCII 英文，避免方块。操作使用 `::`、`->`、`[ok]`、`warning:`、`error:` 和自动对齐表格；颜色只在 TTY 下启用。`--help`、`--version`、`list` 是一等命令。
- **并发下载与彩色进度条**：每个包显示速度、字节数、百分比和 ETA；stderr 不是终端时自动隐藏。
- **LFS 配方**：`hello`、`zlib`、`binutils`、`glibc`、`gcc`、`linux`、`bash`、`coreutils`。

### 环境要求

- Linux，支持 user namespace 和 `unshare`
- Rust 2021 工具链（已在 1.100 上验证）。如果 `cargo` 报“no default toolchain is configured”，执行 `rustup default stable`，或在命令前加 `RUSTUP_TOOLCHAIN=stable`。
- C 工具链（`cc`、`make`），用于 bundled SQLite/zstd/xz
- 仓库签名需要 `gpg`
- 兼容/同步流程需要 `tar`、`gzip`、`zstd`
- 只有可选的 chroot/设备节点实验需要 `sudo`

构建并验证：

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

安装到 `PATH`：

```sh
install -Dm755 target/release/xrpkg ~/.local/bin/xrpkg
# 或系统级
sudo install -Dm755 target/release/xrpkg /usr/local/bin/xrpkg
```

配置好 `config/mirrors.toml` 后，可用 release 二进制做一次同步冒烟测试：

```sh
xrpkg sync --all --jobs 4
```

自动化编译/安装交接提示词见 [docs/AGENT_COMPILE_INSTALL_PROMPT.md](docs/AGENT_COMPILE_INSTALL_PROMPT.md)。

### 磁盘占用

`cargo test` 会在 `target/debug` 下编译 debug 依赖和测试二进制，完整跑一遍可能达到数 GB（调试信息和增量产物）。本工作区实测：`target/debug` 约 **6.9 GiB**，`target/release` 约 **444 MiB**；真正安装的 `xrpkg` 二进制只有 **9.7 MiB**。

安装 release 二进制后，可以这样释放空间：

```sh
# 保留 target/release，删除 debug/测试产物
rm -rf target/debug

# 或删除全部 Cargo 产物（会同时删掉 target/release/xrpkg）
cargo clean
```

磁盘受限的 CI 可以关闭调试信息和增量编译：

```sh
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo test
```

### 快速开始

构建并安装原生包：

```sh
xrpkg --build recipes/hello.toml \
  --root /tmp/xrpkg-root \
  --db /tmp/xrpkg-root/local.db
```

直接安装本地 `.xpkg`：

```sh
xrpkg install ./hello-1.0.xpkg \
  --root /tmp/xrpkg-root \
  --db /tmp/xrpkg-root/local.db
```

默认 Rootfs 是 `/var/tmp/xrpkg-rootfs`。从构建目录运行时默认数据库是 `var/lib/xrpkg/local.db`；安装到 `/usr/bin/xrpkg` 后默认使用 `/var/lib/xrpkg/local.db`。

用 `xrpkg list` 查看已安装包；`xrpkg --help` 显示全部命令。状态变更操作只打印紧凑的事务摘要，不再倾倒整个数据库。

卸载：

```sh
xrpkg --remove hello --root /tmp/xrpkg-root --db /tmp/xrpkg-root/local.db
```

### 构建行为

build/package 默认在沙箱中执行：

```text
/var/tmp/xrpkg-build
/var/tmp/xrpkg-build-sandbox
```

沙箱会：

- 只读绑定宿主 `/usr` 提供工具链；
- 在 `/tmp` 挂载独立 tmpfs；
- 绑定 `/proc`、`/dev`；
- 把源码绑定到 `/src`，把打包输出绑定到 `/pkg`；
- 清空宿主环境变量；
- 只注入 `PATH`、`PKGDIR`、`SRCDIR`、`HOME=/tmp`、`TMPDIR=/tmp`、`MAKEFLAGS`。

默认并发：

```text
MAKEFLAGS=-j8
```

覆盖：

```sh
xrpkg --build recipes/linux.toml --jobs 4
```

配方中也可以写：

```toml
[env]
MAKEFLAGS = "-j6"
```

优先级：命令行 `--jobs` > `[env]` > 默认 `-j8`。

构建日志在项目外：

```text
~/.cache/xrpkg/logs/<name>-build.log
~/.cache/xrpkg/logs/<name>-package.log
```

仅用于调试：

```sh
xrpkg --build recipes/hello.toml --no-sandbox
```

### 源码缓存与临时路径

下载遵循 XDG 缓存规范：

```text
$XDG_CACHE_HOME/xrpkg/downloads
# 或
~/.cache/xrpkg/downloads
```

构建临时目录在 `/var/tmp`：

```text
/var/tmp/xrpkg-build
/var/tmp/xrpkg-build-sandbox
/var/tmp/xrpkg-hooks
/var/tmp/xrpkg-upgrade
```

可以用环境变量改写：

```sh
XRPKG_BUILD_TMP=/tmp xrpkg --build recipes/hello.toml
```

### 仓库与 OpenPGP

仓库配置示例：

```toml
keyring = "/etc/xrpkg/trusted.gpg"

[[repos]]
name = "local"
url = "file:///srv/xrpkg/repo"
```

keyring 中存放导出的公钥：

```sh
gpg --export <KEY_ID> > /etc/xrpkg/trusted.gpg
```

签名包和索引：

```sh
xrpkg repo-add /srv/xrpkg/repo hello-1.0.xpkg --key <KEY_ID>
xrpkg repo-sign <KEY_ID> /srv/xrpkg/repo/xrpkg-repo.toml
```

更新并安装：

```sh
xrpkg update --config ~/.config/xrpkg/repos.toml
xrpkg install hello --config ~/.config/xrpkg/repos.toml
```

`update` 验证 `xrpkg-repo.toml.sig`；`install` 在解包前验证每个 `.xpkg.sig`。缺失签名、签名错误、公钥不在信任环中都会被拒绝。

### 兼容轨道

```sh
xrpkg install ./package.deb
xrpkg install ./package.rpm
xrpkg install ./package.pkg.tar.zst
```

解析 Debian、RPM、Arch 元数据，通过 `config/deps_map.toml` 映射依赖，并把文件与 hooks 记录到 native SQLite 数据库。外部包不会被转换成 `.xpkg`。映射表同时覆盖 Debian/RPM 依赖名和 Arch soname（如 `libz.so`、`libsystemd.so`）。

依赖闭包已在 xrpkg 之外解决时（例如手工缝合的 Arch 闭包），可用 `--no-deps` 只跳过预检；ABI 守门和事务发布仍会执行。

### Arch 仓库同步与安装

在 `config/mirrors.toml` 中配置一个或多个 Arch 仓库目录：

```toml
[[mirrors]]
name = "tuna"
url = "https://mirrors.tuna.tsinghua.edu.cn/archlinux/core/os/x86_64/"
priority = 1
```

同步全部镜像、指定镜像或直接 URL：

```sh
xrpkg sync --all
xrpkg sync tuna
xrpkg sync https://mirrors.ustc.edu.cn/archlinux/core/os/x86_64/
```

`sync` 会并发下载 `.pkg.tar.zst`，校验 SHA256，运行 ABI 守门，并把通过的包以 `origin = "arch"` 写入本地索引。`--jobs <n>` 控制下载线程数，默认是 `min(available_parallelism, 8)`；`--repo-dir <dir>` 可覆盖默认的 `repo/` 输出目录。stderr 不是终端时进度条自动隐藏，不污染 CI 日志。

安装已同步的 Arch 包：

```sh
xrpkg install acl
```

同名原生 `.xpkg` 默认优先；`--prefer-arch` 强制使用 Arch 包。`install` 会从镜像下载 `.pkg.tar.zst`，运行 ABI 守门，发布普通文件和符号链接，并在 SQLite 中记录 `origin = "arch"`。

`sync` 只更新本地索引，不改变签名策略。仍需先用 `xrpkg repo-sign` 签索引，再执行 `xrpkg update`/`install`：

```sh
xrpkg repo-sign <KEY_ID> repo/xrpkg-repo.toml
xrpkg update --config ~/.config/xrpkg/repos.toml
xrpkg install acl
```

### 升级与降级

安装或升级到仓库最新版：

```sh
xrpkg install pkg-a
```

锁定版本：

```sh
xrpkg install pkg-a=1.0
```

允许显式降级：

```sh
xrpkg install pkg-a=1.0 --allow-downgrade
```

没有 `--allow-downgrade` 时，低于已安装版本的目标会在下载前被拒绝。

### 缓存清理

```sh
xrpkg clean
```

保留：

- 数据库中当前安装的版本；
- 当前仓库索引中的最新版本。

删除其他缓存 `.xpkg` 及其 `.sig`，并报告包数、签名数和释放空间。

### Rootfs 初始化与 chroot

生成基础配置：

```sh
xrpkg init --root /tmp/xrpkg-root
```

会创建 `/etc/passwd`、`/etc/group`、`/etc/hosts`、`/etc/resolv.conf`、`/etc/nsswitch.conf`、`/etc/ld.so.conf`，以及 `/bin`、`/sbin`、`/lib`、`/lib64` 符号链接。

chroot 测试前需要安装可用的 glibc 和运行库，然后创建设备节点：

```sh
sudo mknod -m 666 /tmp/xrpkg-root/dev/null c 1 3
sudo mknod -m 600 /tmp/xrpkg-root/dev/console c 5 1
sudo chroot /tmp/xrpkg-root /bin/bash
```

### 项目结构

```text
src/
  main.rs        CLI 与命令分发
  abi.rs         ELF GNU 符号版本与 GLIBC 基线校验
  capability.rs  版本解析与包能力约束
  builder.rs     源码配方、沙箱、日志
  installer.rs   暂存、ABI 校验、升级事务
  fs_meta.rs     权限、属主、扩展属性与 PAX 解码
  transaction.rs 原子发布、快照、日志与崩溃恢复
  uninstall.rs   基于数据库的精确卸载与空目录清理
  world.rs       recipes/system.toml 编排（build-world）
  hooks.rs       生命周期 hooks 与 hook 沙箱
  repo.rs        仓库索引、缓存、下载、清理
  download.rs    统一下载与进度条
  ui.rs          多语言 CLI 输出、表格、帮助与颜色策略
  mirrors.rs     config/mirrors.toml 加载与优先级解析
  sync.rs        Arch core.db 同步与并发入库
  sign.rs        OpenPGP 分离签名
  state.rs       SQLite 状态
  metadata.rs    场景/仓库 TOML 加载
  deps_map.rs    外部依赖与 soname 映射
  solver.rs      玩具求解器（resolvo 对照基线）
  resolvo_solver.rs 依赖求解
  xpkg.rs        .xpkg 读写与安全解包
  compat_*.rs    deb/rpm/arch 兼容轨道
recipes/         LFS 构建配方
config/          仓库配置、依赖映射与 mirrors.toml
tests/           集成测试
repo/            构建产物（已忽略）
var/             本地 SQLite 状态（已忽略）
```

### 配方

| 配方 | 版本 |
|---|---|
| `hello` | 1.0 |
| `zlib` | 1.3.1 |
| `binutils` | 2.42 |
| `glibc` | 2.39 |
| `gcc` | 13.2.0 |
| `linux` | 6.7.4 |
| `bash` | 5.2.21 |
| `coreutils` | 9.4 |

> ABI 守门基线是 `GLIBC_2.44`，但仓库内已提交的 glibc 配方构建的是 2.39。`repo/glibc-2.44.xpkg` 不是由这套配方产出的，当前无法从本仓库重建；见 `PROGRESS.md` 的“已知差距”。

### 测试

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

当前本地结果：**32 个测试目标、174 个测试通过**。部分测试需要 `unshare`、user namespace、`gpg`、`setcap`（root-only capability 用例）和 `zstd`。

测试二进制共用 `~/.cache/xrpkg`，并行运行时偶发 `Text file busy`（ETXTBSY）。删除 `~/.cache/xrpkg/downloads/fetched-*` 后重跑即可；让每个测试使用独立缓存目录是已知的后续改进。

### 安全模型与边界

`xrpkg` 会校验：

- 仓库索引签名；
- 包签名；
- 包 SHA-256；
- ELF 的 GLIBC 符号版本；
- 归档路径和符号链接目标；
- 删除共享文件前的数据库归属。

构建沙箱隔离 mount 和文件系统视图，但它不是虚拟机，也没有创建独立 network namespace。不要把不可信配方当作安全代码执行。

### 限制

- 尚未达到生产级。
- ABI 基线是 `GLIBC_2.44`，但仓库内已提交的 glibc 配方构建的是 2.39；`repo/glibc-2.44.xpkg` 当前无法从本仓库重建。
- LFS glibc/gcc 配方已包含并验证 configure；完整工具链构建耗时长且依赖硬件。
- 兼容轨道会把外部包记录到 native 数据库，但不会重写其依赖元数据。
- `sync` 不会自动签名仓库；执行 `xrpkg update` 前仍需走既有 `repo-sign` 流程。
- 本阶段只同步 Arch/Pacman 生态，尚未实现 apt/dnf。
- 仓库签名依赖系统 `gpg`。
- 仓库内 `repo/xrpkg-repo.toml` 只索引了 `repo/` 的一部分产物；把它当完整索引前先用 `repo-add` 重新生成。
- 测试共用 `~/.cache/xrpkg`，并行执行时可能相互干扰（见“测试”）。

[English](#english)