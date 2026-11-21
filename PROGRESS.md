# 项目进度

## 已完成的里程碑 (Completed)
- [x] 阶段 1-19：基础 CLI、SQLite 状态库、依赖映射、GPG 签名、原生 `.xpkg` 格式、ABI 守门员、原子升级/降级。
- [x] 阶段 20-26：LFS 核心包构建（GCC、Glibc、Linux 6.7.4），并成功部署到物理 NVMe 分区（/dev/nvme0n1）。
- [x] 阶段 27：内核排雷（强制内置 ext4/NVMe 驱动，修复 PARTUUID 启动问题）。
- [x] 阶段 28：成功引导进入自建的 XROS beta1.0 系统。并使用兼容轨道白嫖了 Arch 的 systemd、NetworkManager、util-linux。
- [x] 阶段 28.5：在 XROS 内部成功运行 xrpkg 二进制。
- [x] 阶段 29.5、30、32：CLI 输出标准化与多语言、事务/元数据加固、XFCE/Xorg 兼容安装（阶段 31 仍在进行）。

## 当前架构环境
- **宿主机**：VirtualBox 中的 Arch Linux（通过 VS Code Remote-SSH 连接）。
- **目标系统**：XROS beta1.0（运行在 NVMe 硬盘 `/dev/nvme0n1` 上，已完美启动并拥有 root 权限）。
- **核心工具**：`xrpkg` 已在 XROS 内可用，但原生的系统包还很少，主要依赖兼容轨道（缝合 Arch 包）。

## CLI 输出标准化与多语言（阶段 29.5）
- 新增 `src/ui.rs`：集中管理语言、字符集、颜色、状态前缀、表格和帮助。
- 语言依据 `LC_ALL > LC_MESSAGES > LANG`；`zh*` + UTF-8 输出中文，其余输出英文；`zh*` 但终端不是 UTF-8 时回退 ASCII 英文，避免方块。
- `XRPKG_LANG=zh_CN.UTF-8|en_US.UTF-8` 可显式覆盖语言，`XRPKG_ASCII=1` 强制 ASCII 英文，`NO_COLOR=1` 禁用颜色。
- 统一前缀：`::` 阶段、`  ->` 步骤、`  [ok]` 成功、`warning:`、`error:`；不使用 Unicode 框线或 Emoji。
- 新增 `--help`/`-h`、`--version`/`-V`、`list` 命令；无参数时显示帮助，不再默认安装 demo 包。
- 安装计划、搜索、已安装包改用自动对齐表格；状态变更后不再倾倒整个数据库，只打印事务摘要，完整列表用 `xrpkg list`。
- 端到端测试覆盖中文、英文、非 UTF-8 回退、显式覆盖、ASCII 覆盖、空参数帮助和版本输出；当前 32 个测试目标、174 个测试通过。

## 阶段 30：事务与元数据加固（已完成）
- **元数据保真**：新增 `src/fs_meta.rs`，TAR 解包与文件发布完整保留 SUID/SGID、属主和扩展属性；同时解析 GNU tar `SCHILY.xattr.*` 与 libarchive `LIBARCHIVE.xattr.*`（Base64），`security.capability` 不再被丢弃。
- **原生打包**：`builder` 写包时保留权限位和 xattrs；非 root 构建统一把归档属主归一化为 root，避免把开发用户的 UID 带进目标系统。
- **原子发布**：新增 `src/transaction.rs`。每个文件先写同目录临时文件、应用完整元数据并 `fsync`，再 `rename(2)` 覆盖目标；旧文件用硬链接/改名快照。
- **崩溃恢复**：事务日志写入 `<rootfs>/.xrpkg-txn/<id>/journal`。下次事务开始前按日志和数据库状态判定提交或回滚，覆盖数据库提交前后的断电窗口。
- **卸载/升级回滚**：卸载先把文件移入快照，`post_remove` 或数据库提交失败时原样恢复；外部 `.deb`/`.rpm`/`.pkg.tar.zst` 升级复用同一事务，失败恢复旧文件和旧版本记录。卸载前还会检查目标 Rootfs 是否至少包含一个该包的独占文件，避免 `--root` 指错时只删数据库记录。
- **本地安装**：`xrpkg install ./foo.xpkg` 已接入统一求解/ABI/事务提交路径。
- **world 编排**：`build-world` 已完整接入 `main.rs`，clippy 与 fmt 保持全绿。
- **XROS 实机**：通过挂载 `/dev/nvme0n1p3` 安装 `noto-fonts-cjk 20240730-1`（14 个 TTC，约 299 MiB），刷新 fontconfig 缓存；新构建的 `xrpkg` 部署到 `/usr/bin/xrpkg`，旧二进制备份为 `/usr/bin/xrpkg.old`。
- **测试基线**：32 个测试目标、174 个测试通过，clippy `-D warnings` 与 `cargo fmt --check` 全绿；capability 用例在 root 下单独验证通过。

## 阶段 32：XFCE/Xorg 兼容安装（实现已完成，XROS 端到端待验证）
- **发布覆盖链接**：目标位置已有符号链接/硬链接时，只删除链接本身再创建新文件，绝不通过链接写穿到链接目标；目标为目录仍然拒绝覆盖。
- **悬空符号链接**：允许包内符号链接先指向尚不存在的路径（由同批后续文件创建），但必须通过“最近的已存在祖先仍在 Rootfs 内”的检查，逃逸目标依旧拒绝。
- **依赖闭包外置**：新增 `--no-deps`，用于依赖闭包已经在 xrpkg 之外解决（手工缝合 Arch 包）的场景；只跳过预检，ABI 守门和事务发布仍然执行。
- **soname 映射**：`config/deps_map.toml` 增加 XFCE/Xorg 需要的 soname/虚拟依赖到 Arch 提供包的映射（glib2、libglvnd、systemd-libs、freetype2 等）。
- **内核配方**：`recipes/linux.toml` 启用 DRM/VBOXVIDEO/VWMWFX 和 framebuffer console，内核模块安装到 `/usr/lib/modules`。
- **测试**：新增 `publish_replaces_existing_file_with_hardlink`、`publish_replaces_symlinks_without_following_them`、`allows_dangling_symlink_target_for_later_package_files` 等用例；全量 174 个测试通过。

## 当前任务 (In Progress)
**阶段 31：生产可用性收尾**
- RPM 包头的 `%caps` 文本尚未转换为 `security.capability` 二进制；当前优先保证 Arch `.pkg.tar.zst` 与原生 `.xpkg` 的 capability 保真。
- `Rootfs::prepare` 仍拒绝 `/`，真实系统写入需从宿主挂载目标分区后使用 `--root <挂载点>`；是否开放受控的 `--root /` 需要单独的安全设计。
- 下一步补充真实断电注入、只读目标盘、并发 xrpkg 进程锁和安装后 `ldconfig`/fontconfig 钩子的回归测试。
- 测试隔离：多个测试二进制并发共用 `~/.cache/xrpkg`，偶发 `Text file busy`（ETXTBSY）；清理 `~/.cache/xrpkg/downloads/fetched-*` 后可恢复，长期应改为每个测试使用独立缓存目录。

## 已知差距 (Known Gaps)
- 配方落后于基线：`recipes/glibc.toml` 仍是 2.39，`recipes/linux.toml` 是 6.7.4；`repo/glibc-2.44.xpkg` 存在，但无法用当前仓库内的配方重建。
- `repo/xrpkg-repo.toml` 只索引了 `repo/` 的一部分产物（8 个包），与目录内容不同步；在重新生成索引前不要把它当作完整仓库清单。
- 文档已于 2026-11-21 对齐到阶段 32；此前 `AGENTS.md` 的“仅标准库/禁止 resolvo”和旧测试计数均已修正。