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
#
# `make debug CORE=<core.zst> [ELF=<kernel.elf>]` starts no QEMU: the core
# tool writes the core's virtually addressed core to build/debug/core.virt,
# symbols.gdb opens it (`$vibeos_core` 1), and this script attaches to no
# stub (ROADMAP §10.7). `info threads` lists one thread per CPU.

set confirm off
set pagination off
set architecture i386:x86-64
source build/debug/symbols.gdb
if $vibeos_core == 0
  target remote localhost:1234
end
