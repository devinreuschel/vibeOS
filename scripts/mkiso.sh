#!/usr/bin/env bash
# Assemble a hybrid BIOS+UEFI ISO. Invoked by KERNEL_VARIANT (B1).
# usage: mkiso.sh <kernel-elf> <out.iso> <staging-dir>
#
# Reproducible (ROADMAP §10.2, F152; DESIGN §3.6): every time in the image
# comes from SOURCE_DATE_EPOCH, and Rock Ridge records no builder uid or gid.
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

# 1. The epoch: the caller's SOURCE_DATE_EPOCH (the Makefile exports the
# commit's), else the commit time, else 2010-01-01. The stamp is its UTC time
# in the volume descriptor's YYYYMMDDhhmmsscc form.
epoch=${SOURCE_DATE_EPOCH:-}
if [ -z "$epoch" ]; then
    epoch=$(git -C "$root" log -1 --format=%ct 2>/dev/null || true)
fi
epoch=${epoch:-1262304000}
case $epoch in
    '' | *[!0-9]*)
        echo "mkiso.sh: SOURCE_DATE_EPOCH=$epoch is not a decimal number of seconds" >&2
        exit 2
        ;;
esac
stamp=$(python3 -c 'import sys, time; print(time.strftime("%Y%m%d%H%M%S00", time.gmtime(int(sys.argv[1]))))' "$epoch")

# 2. Staging: everything the ISO holds. Add new files here, above the time
# pin, so their times are pinned too.
rm -rf "$staging"
mkdir -p "$staging/boot" "$staging/EFI/BOOT"
cp "$kernel_elf" "$staging/boot/vibeos"
cp "$root/limine.conf" "$staging/boot/"
cp "$limine_dir/limine-bios.sys" "$staging/boot/"
cp "$limine_dir/limine-bios-cd.bin" "$staging/boot/"
cp "$limine_dir/limine-uefi-cd.bin" "$staging/boot/"
cp "$limine_dir/BOOTX64.EFI" "$staging/EFI/BOOT/"

# 3. The time pin: with SOURCE_DATE_EPOCH set, xorriso takes file times from
# the staged files, so every staged path gets the epoch.
python3 - "$staging" "$epoch" <<'PY'
import os
import sys

top, t = sys.argv[1], int(sys.argv[2])
for d, dirs, files in os.walk(top):
    for name in dirs + files:
        os.utime(os.path.join(d, name), (t, t), follow_symlinks=False)
os.utime(top, (t, t))
PY

# 4. The image: -r records uid and gid 0 and sane modes; the volume dates and
# UUID come from the stamp.
TZ=UTC xorriso -as mkisofs -quiet \
    -r \
    --modification-date="$stamp" \
    --set_all_file_dates "$stamp" \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$staging" -o "$out_iso"

# 5. Limine's BIOS stages.
"$limine_dir/limine" bios-install "$out_iso" >/dev/null
echo "  ISO $out_iso"
