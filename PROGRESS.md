# 项目进度

## 已完成的里程碑 (Completed)
- [x] 阶段 1-19：基础 CLI、SQLite 状态库、依赖映射、GPG 签名、原生 `.xpkg` 格式、ABI 守门员、原子升级/降级。
- [x] 阶段 20-26：LFS 核心包构建（GCC、Glibc、Linux 6.7.4），并成功部署到物理 NVMe 分区（/dev/nvme0n1）。
- [x] 阶段 27：内核排雷（强制内置 ext4/NVMe 驱动，修复 PARTUUID 启动问题）。
- [x] 阶段 28：成功引导进入自建的 XROS beta1.0 系统。并使用兼容轨道白嫖了 Arch 的 systemd、NetworkManager、util-linux。
- [x] 阶段 28.5：在 XROS 内部成功运行 xrpkg 二进制。
- [x] 阶段 29.5、30、31、32：CLI 输出标准化与多语言、事务/元数据加固、生产可用性收尾、XFCE/Xorg 兼容安装。

## 当前架构环境
- **开发环境**：原生 Arch Linux（内核 `7.3.5-arch1-1`，x86_64）。项目最早在 WSL2 上开发，现已迁移；WSL/Windows 路径说明全部废弃。
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

## 阶段 31：生产可用性收尾（已完成）
- **并发进程锁**：新增 `src/lock.rs`。`install`、`remove`、`--build`、`build-world`、`init` 在打开数据库/写文件之前按 Rootfs + 数据库路径获取 `flock(2)` 排他锁；第二个进程立即失败且不产生部分写入，不同目标可并行，只读命令不加锁。锁文件位于 `$XDG_RUNTIME_DIR/xrpkg/locks` 或 `/tmp/xrpkg-locks-<uid>`，不进入包清单，进程崩溃后由内核释放。
- **安装后触发器**：新增 `src/triggers.rs`。事务和数据库提交后，根据本次发布及升级删除的文件判定是否执行 `ldconfig`/`fc-cache`；每个触发器每次事务最多一次；工具取自目标 Rootfs，并在 hooks 的 chroot 沙箱内执行；工具缺失或退出非 0 只打印 `warning:`，不回滚已提交事务；`--no-hooks` 连触发器一起跳过。
- **RPM %caps**：新增 `src/rpm_caps.rs`，把 RPM `%caps` 文本解析为 Linux VFS `security.capability` 字节（REVISION_2/3、`all`、大小写不敏感、`=`/`+`/`-` 顺序应用）；`compat_rpm` 在普通文件上写入该 xattr；非法文本返回明确错误；非 root 写入失败让安装失败并回滚。
- **测试缓存隔离**：`tests/common::isolate_cache()` 为每个测试二进制设置独立临时 `XDG_CACHE_HOME`。连续 3 次全量 `cargo test` 全绿，测试后 `~/.cache/xrpkg` 没有新增文件，ETXTBSY 不再出现。
- **测试基线**：35 个测试目标、201 个测试通过；`cargo fmt --check` 与 `cargo clippy --all-targets -- -D warnings` 全绿。新增 `tests/locking.rs`、`tests/triggers.rs`、`tests/rpm_caps.rs` 及模块内单元测试。
- **实测**：在真实 glibc 2.44 Rootfs 上安装 zlib，目标 `/usr/sbin/ldconfig -p` 立即列出 `libz.so.1`；并发锁手工测试中第二个进程报“另一个 xrpkg 进程”，第一个进程正常完成。

## 阶段 32：XFCE/Xorg 兼容安装（实现已完成，XROS 端到端待验证）
- **发布覆盖链接**：目标位置已有符号链接/硬链接时，只删除链接本身再创建新文件，绝不通过链接写穿到链接目标；目标为目录仍然拒绝覆盖。
- **悬空符号链接**：允许包内符号链接先指向尚不存在的路径（由同批后续文件创建），但必须通过“最近的已存在祖先仍在 Rootfs 内”的检查，逃逸目标依旧拒绝。
- **依赖闭包外置**：新增 `--no-deps`，用于依赖闭包已经在 xrpkg 之外解决（手工缝合 Arch 包）的场景；只跳过预检，ABI 守门和事务发布仍然执行。
- **soname 映射**：`config/deps_map.toml` 增加 XFCE/Xorg 需要的 soname/虚拟依赖到 Arch 提供包的映射（glib2、libglvnd、systemd-libs、freetype2 等）。
- **内核配方**：`recipes/linux.toml` 启用 DRM/VBOXVIDEO/VWMWFX 和 framebuffer console，内核模块安装到 `/usr/lib/modules`。
- **测试**：新增 `publish_replaces_existing_file_with_hardlink`、`publish_replaces_symlinks_without_following_them`、`allows_dangling_symlink_target_for_later_package_files` 等用例；当时全量 174 个测试通过。

## 整体优化（2026-11-21）
- **兼容轨道符号链接**：`.deb`/`.rpm` 的相对符号链接改为通过统一的 `create_symlink` 安装，沿用 Rootfs 逃逸校验（悬空目标允许、绝对目标拒绝）；新增 4 个集成测试。
- **暂存目录统一**：新增 `src/staging.rs`，`main.rs` 与 `installer.rs` 共用同一实现；目录名冲突时自动重试，`Drop` 统一清理。
- **锁目录回退**：`XDG_RUNTIME_DIR` 不可写或不是绝对路径时自动回退 `/tmp/xrpkg-locks-<uid>`；`XRPKG_LOCK_DIR` 可显式覆盖，README 已记录。
- **帮助文案**：`--no-hooks` 明确说明同时跳过安装后缓存触发器（中英双语）。
- **工具链**：新增 `rust-toolchain.toml`（stable），本机不再需要手动设置 `RUSTUP_TOOLCHAIN`，普通 `cargo` 命令即可工作。
- **测试基线**：35 个测试目标、206 个测试通过；fmt/clippy 全绿。

## 当前任务 (In Progress)
**下一阶段待定**
- `Rootfs::prepare` 仍拒绝 `/`，真实系统写入需从宿主挂载目标分区后使用 `--root <挂载点>`；是否开放受控的 `--root /` 需要单独的安全设计（本阶段非目标）。
- 真实断电注入、只读目标盘的回归测试尚未实现；并发锁与安装后触发器已在本阶段完成。
- RPM `%caps` 的 root 端到端 `getcap` 验证需要 root 环境；非 root 时只能验证解析结果与失败/回滚路径。

## 已知差距 (Known Gaps)
- 配方落后于基线：`recipes/glibc.toml` 仍是 2.39，`recipes/linux.toml` 是 6.7.4；`repo/glibc-2.44.xpkg` 存在，但无法用当前仓库内的配方重建。
- `repo/xrpkg-repo.toml` 只索引了 `repo/` 的一部分产物（8 个包），与目录内容不同步；在重新生成索引前不要把它当作完整仓库清单。
- `.deb`/`.rpm` 的绝对符号链接与 `.deb` 硬链接尚未支持；相对符号链接已覆盖。
- 文档已对齐到阶段 31/32 的实际状态；此前 `AGENTS.md` 的“仅标准库/禁止 resolvo”和旧测试计数均已修正。