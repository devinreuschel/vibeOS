# gdb setup for a `make debug` session (DESIGN §8.4).
#
# `make debug` starts QEMU halted with a gdb stub on localhost:1234 and writes
# build/debug/symbols.gdb, which loads the kernel ELF and the initrd's user
# ELFs. Run gdb from the repository root, since the path below is relative:
#
#     gdb -x scripts/vibeos.gdb
#
# On macOS use Homebrew's x86_64-elf-gdb; Homebrew has no gdb on Apple Silicon.
# The kernel is not in memory when QEMU starts (Limine loads it later), so a
# software breakpoint cannot be planted yet: use a hardware one, then continue.
#
#     hbreak _start
#     continue

set confirm off
set pagination off
set architecture i386:x86-64
source build/debug/symbols.gdb
target remote localhost:1234
