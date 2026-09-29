#!/usr/bin/env bash
# Assemble a hybrid BIOS+UEFI ISO. Invoked by KERNEL_VARIANT (B1).
# usage: mkiso.sh <kernel-elf> <initrd> <out.iso> <staging-dir>
#
# Reproducible (ROADMAP §10.2, F152; DESIGN §3.6): every time in the image
# comes from SOURCE_DATE_EPOCH, Rock Ridge records no builder uid or gid, and
# no identifier is random.
set -euo pipefail

# The GPT disk GUID; xorriso derives the partition GUIDs from it. Any fixed
# value does: nothing looks the disk up by it.
GPT_DISK_GUID=76696265-4f53-4953-8f00-000000000001

if [ "$#" -ne 4 ]; then
    echo "usage: mkiso.sh <kernel-elf> <initrd> <out.iso> <staging-dir>" >&2
    exit 2
fi

kernel_elf=$1
initrd=$2
out_iso=$3
staging=$4

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
# The notices every published image carries (ROADMAP §10.9, DESIGN §1.5):
# vibeOS's LICENSE and the third-party notices, generated for this tree.
mkdir -p "$staging/LICENSES"
cp "$root/LICENSE" "$staging/LICENSES/LICENSE"
python3 "$root/scripts/gen_notices.py" --out "$staging/LICENSES/THIRD-PARTY-NOTICES.txt" \
    --limine-dir "$limine_dir"
cp "$kernel_elf" "$staging/boot/vibeos"
# The initrd, which limine.conf's module_path: loads as a Limine module.
cp "$initrd" "$staging/boot/initrd.fat"
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
# UUID come from the stamp, and the GPT GUIDs from the constant.
TZ=UTC xorriso -as mkisofs -quiet \
    -r \
    --modification-date="$stamp" \
    --set_all_file_dates "$stamp" \
    --gpt_disk_guid "$GPT_DISK_GUID" \
    -b boot/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    --efi-boot boot/limine-uefi-cd.bin \
    -efi-boot-part --efi-boot-image --protective-msdos-label \
    "$staging" -o "$out_iso"

# 5. Limine's BIOS stages.
"$limine_dir/limine" bios-install "$out_iso" >/dev/null

# 6. bios-install seeds the MBR disk signature at 0x1B8 from time(NULL); the
# signature becomes one derived from the image. limine.conf names its files
# with boot(), so nothing looks the disk up by it.
python3 "$root/scripts/iso_disk_id.py" "$out_iso"

# 7. The xorriso version, which lands in the volume descriptor, recorded
# beside the ISO for a release to publish. sed, not head: SIGPIPE would trip
# pipefail.
xorriso -version 2>&1 | sed -n 1p > "$out_iso.xorriso-version"
echo "  ISO $out_iso"
