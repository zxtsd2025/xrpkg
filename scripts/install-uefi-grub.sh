#!/usr/bin/env bash
# 在 Arch 宿主机上为 XROS beta1.0 Rootfs 安装 UEFI GRUB，并生成最小 grub.cfg。
#
# 用法（必须 root）:
#   scripts/install-uefi-grub.sh [/mnt/lfs] [/dev/nvme0n1] [/dev/nvme0n1p3] [6.7.4]
#
# 说明：
# - 第一条 grub-install 使用任务要求的完整参数；
# - 若当前固件不允许写 NVRAM，则追加 --no-nvram 重试；
# - 最后把 grubx64.efi 复制到 EFI/BOOT/BOOTX64.EFI，保证换机器/虚拟机时
#   仍能通过 UEFI 可移动路径启动。
set -euo pipefail

ROOT="${1:-/mnt/lfs}"
DISK="${2:-/dev/nvme0n1}"
ROOT_PART="${3:-/dev/nvme0n1p3}"
KVER="${4:-6.7.4}"
LOG="${XRPKG_GRUB_LOG:-/var/log/xrpkg-grub-install.log}"

die() {
    echo "错误: $*" >&2
    exit 1
}

[[ ${EUID} -eq 0 ]] || die "需要 root 权限才能执行 grub-install"
[[ -d "$ROOT/boot/efi" ]] || die "EFI 分区未挂载: $ROOT/boot/efi"
[[ -d "$ROOT/boot" ]] || die "目标 /boot 不存在: $ROOT/boot"
command -v blkid >/dev/null 2>&1 || die "找不到 blkid"

# 优先使用带版本的内核；LFS 的 make install 也可能落成无版本 vmlinuz/bzImage。
KERNEL_IMAGE=""
for candidate in "vmlinuz-$KVER" "vmlinuz" "bzImage"; do
    if [[ -f "$ROOT/boot/$candidate" ]]; then
        KERNEL_IMAGE="/boot/$candidate"
        break
    fi
done
[[ -n "$KERNEL_IMAGE" ]] || die "在 $ROOT/boot 下找不到 vmlinuz-$KVER、vmlinuz 或 bzImage"

ROOT_UUID="$(blkid -s UUID -o value "$ROOT_PART")"
ROOT_PARTUUID="$(blkid -s PARTUUID -o value "$ROOT_PART")"
[[ -n "$ROOT_UUID" ]] || die "无法从 $ROOT_PART 读取 UUID"
[[ -n "$ROOT_PARTUUID" ]] || die "无法从 $ROOT_PART 读取 PARTUUID"

# 只在脚本内部打开日志；终端仍能看到完整输出。
exec > >(tee -a "$LOG") 2>&1
echo "==> xrpkg UEFI GRUB 安装开始: $(date -Is)"
echo "==> Rootfs=$ROOT Disk=$DISK RootPart=$ROOT_PART Kernel=$KERNEL_IMAGE"
echo "==> Root UUID=$ROOT_UUID PARTUUID=$ROOT_PARTUUID"

# 宿主机需要 grub 与 efibootmgr。目标机没有包管理器，因此使用 Arch 宿主工具。
if ! command -v grub-install >/dev/null 2>&1; then
    echo "==> 安装宿主机依赖: grub efibootmgr"
    pacman -S --needed --noconfirm grub efibootmgr
fi

echo "==> 执行 grub-install"
if ! grub-install \
    --target=x86_64-efi \
    --efi-directory="$ROOT/boot/efi" \
    --boot-directory="$ROOT/boot" \
    --root-directory="$ROOT" \
    --bootloader-id=XROS \
    "$DISK"; then
    echo "==> grub-install 写入 NVRAM 失败，使用 --no-nvram 重试" >&2
    grub-install \
        --target=x86_64-efi \
        --efi-directory="$ROOT/boot/efi" \
        --boot-directory="$ROOT/boot" \
        --root-directory="$ROOT" \
        --bootloader-id=XROS \
        --no-nvram \
        "$DISK"
fi

echo "==> 生成 $ROOT/boot/grub/grub.cfg"
install -d -m 0755 "$ROOT/boot/grub"
cat >"$ROOT/boot/grub/grub.cfg" <<EOF
# 由 xrpkg scripts/install-uefi-grub.sh 生成。
set default=0
set timeout=5

insmod part_gpt
insmod ext2
insmod fat
insmod linux
insmod efi_gop
insmod efi_uga
insmod all_video
set gfxmode=auto
terminal_output gfxterm

search --no-floppy --fs-uuid --set=root $ROOT_UUID

menuentry "XROS beta1.0 / Linux $KVER (VirtualBox console)" {
    linux $KERNEL_IMAGE root=PARTUUID=$ROOT_PARTUUID rw rootfstype=ext4 console=tty0
}

menuentry "XROS beta1.0 / Linux $KVER (serial console)" {
    linux $KERNEL_IMAGE root=PARTUUID=$ROOT_PARTUUID rw rootfstype=ext4 console=ttyS0,115200
}

menuentry "XROS beta1.0 / rescue shell (init=/usr/bin/bash)" {
    linux $KERNEL_IMAGE root=PARTUUID=$ROOT_PARTUUID rw rootfstype=ext4 console=tty0 console=ttyS0,115200 init=/usr/bin/bash
}
EOF

# 优先使用 XROS 的 EFI 目录；旧配置或宿主 distributor 可能留下其他目录。
GRUB_EFI="$ROOT/boot/efi/EFI/XROS/grubx64.efi"
if [[ ! -f "$GRUB_EFI" ]]; then
    GRUB_EFI="$(find "$ROOT/boot/efi/EFI" -mindepth 2 -maxdepth 2 -name grubx64.efi -print -quit 2>/dev/null || true)"
fi
if [[ -n "$GRUB_EFI" ]]; then
    # 兼容 UEFI 可移动路径，避免换机器后 NVRAM 中没有 GRUB 条目。
    install -d -m 0755 "$ROOT/boot/efi/EFI/BOOT"
    install -m 0644 "$GRUB_EFI" "$ROOT/boot/efi/EFI/BOOT/BOOTX64.EFI"
    echo "==> 已安装 UEFI 回退路径 EFI/BOOT/BOOTX64.EFI（来源 $GRUB_EFI）"
else
    echo "警告: 没有找到 grubx64.efi，跳过 EFI/BOOT/BOOTX64.EFI 回退" >&2
fi

echo "==> 校验产物"
if [[ -n "$GRUB_EFI" ]]; then
    ls -l "$GRUB_EFI" "$ROOT/boot/grub/grub.cfg"
else
    ls -l "$ROOT/boot/grub/grub.cfg"
fi
if command -v efibootmgr >/dev/null 2>&1; then
    efibootmgr -v || true
fi
sync
echo "==> xrpkg UEFI GRUB 安装完成: $(date -Is)"
