#!/usr/bin/env bash
# 一键完成阶段 26 的目标系统部署：初始化 Rootfs、部署包、生成 fstab、安装 GRUB。
#
# 用法（必须 root）:
#   scripts/deploy-uefi.sh [/mnt/lfs] [/dev/nvme0n1] [8]
#
# 该脚本只操作参数指定的目标盘/Rootfs，不会执行 swapon，也不会修改宿主机 fstab。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="${1:-/mnt/lfs}"
DISK="${2:-/dev/nvme0n1}"
JOBS="${3:-8}"
XRPKG_BIN="${XRPKG_BIN:-$PROJECT_ROOT/target/release/xrpkg}"

die() {
    echo "错误: $*" >&2
    exit 1
}

[[ ${EUID} -eq 0 ]] || die "需要 root 权限才能写 $ROOT 并执行 grub-install"
[[ -x "$XRPKG_BIN" ]] || die "找不到可执行文件 $XRPKG_BIN；请先 cargo build --release"
[[ -d "$ROOT" ]] || die "目标 Rootfs 不存在: $ROOT"
[[ -d "$ROOT/boot/efi" ]] || die "EFI 分区未挂载: $ROOT/boot/efi"
command -v findmnt >/dev/null 2>&1 && {
    findmnt -n "$ROOT" >/dev/null || die "$ROOT 未挂载"
    findmnt -n "$ROOT/boot/efi" >/dev/null || die "$ROOT/boot/efi 未挂载"
}

cd "$PROJECT_ROOT"

echo "==> [1/4] 初始化 Rootfs 基础目录与 /etc"
"$XRPKG_BIN" init --root "$ROOT"

echo "==> [2/4] build-world: 构建并安装核心包（已存在的 .xpkg 自动复用）"
"$XRPKG_BIN" build-world \
    --config recipes/system.toml \
    --root "$ROOT" \
    --jobs "$JOBS"

echo "==> [3/4] 生成 fstab"
"$SCRIPT_DIR/gen-fstab.sh" "$ROOT"

echo "==> [4/4] 安装 UEFI GRUB 并生成 grub.cfg"
"$SCRIPT_DIR/install-uefi-grub.sh" "$ROOT" "$DISK"

echo "==> 阶段 26 部署完成"
