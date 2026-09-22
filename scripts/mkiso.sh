#!/usr/bin/env bash
# Assemble a hybrid BIOS+UEFI ISO. Invoked by KERNEL_VARIANT (B1).
# usage: mkiso.sh <kernel-elf> <out.iso> <staging-dir>
set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "usage: mkiso.sh <kernel-elf> <out.iso> <staging-dir>" >&2
    exit 2
fi

kernel_elf=$1
out_iso=$2
staging=$3

root=$(cd "$(dirname "$0")/.." && pwd)
limine_dir=${LIMINE_DIR:-"$root/limine"}

rm -rf "$staging"
mkdir -p "$staging/boot" "$staging/EFI/BOOT"
cp "$kernel_elf" "$staging/boot/vibeos"
cp "$root/limine.conf" "$staging/boot/"
cp "$limine_dir/limine-bios.sys" "$staging/boot/"
cp "$limine_dir/limine-bios-cd.bin" "$staging/boot/"
cp "$limine_dir/limine-uefi-cd.bin" "$staging/boot/"
cp "$limine_dir/BOOTX64.EFI" "$staging/EFI/BOOT/"
xorriso -as mkisofs -quiet \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$staging" -o "$out_iso"
"$limine_dir/limine" bios-install "$out_iso" >/dev/null
echo "  ISO $out_iso"
