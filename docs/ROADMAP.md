# Roadmap

Where this goes. The first destination is a self-hosting operating system: one that boots on QEMU's
models of real machines on both architectures, runs a graphical userspace, has a network stack you can
serve from, and can compile and test its own source tree on itself. Past it, vibeOS runs the Linux
software people already have, runs production server workloads under QEMU, Firecracker,
cloud-hypervisor, and OpenVMM, becomes a desktop someone could use every day in a VM, and reaches a 1.0
whose interfaces hold still and whose allocator and page tables are proved. Every gate runs on free
infrastructure: hosted CI runners, QEMU, and the maintainer's Mac as a VM host. Real hardware, public
clouds, and bare-metal laptops and desktops are [funded goals](#funded-goals). Written by agents.

That is absurd. Good. The interesting failures happen past the point where the tutorials stop.

## How to read this

Forty phases in eight eras, then a list of what comes after, then a list of what money would add.
Ordering is by dependency, not by preference: a phase's exit gate is the thing the next phase assumes.
Within a phase, parts are mostly parallelizable. Where two phases are independent, the era preamble says
so; the numbers are not a queue.

**Two architectures.** x86_64 and aarch64 are both first class from [Phase 11](#phase-11-portability)
on. A gate is met on both, or the phase says which lines are single-architecture and why. From Phase 12
on, every phase has an **Architectures** line that says which. x86_64 came first and is the reference
when they disagree.

**Linux interfaces.** Where Linux defines a kernel interface that vibeOS also offers (a syscall, an
`ioctl`, a `/proc` or `/sys` file, a netlink family, the protocol of a device node), vibeOS implements
Linux's, in each architecture's layout, so unmodified Linux software runs; §13.11 and
[Phase 23](#phase-23-linux-compatibility) test it against Linux. "Linux's" means the interface of one
baseline release, a Linux LTS that `docs/LINUX.md` names (§13.11 names the first, and from Phase 23 it is
the §23.6 reference kernel's release); newer Linux behaviour arrives by moving the baseline in an edit
that lists what changed. A native interface or a deliberate
divergence is listed with its reason in `docs/LINUX.md` (§13.11), or has a line in this file naming the
phase that replaces it. A native interface is a `/proc` or `/sys` file, an `ioctl` on a vibeOS device
node, or a generic-netlink family, never a new syscall number: Linux allocates numbers as it goes, so a
number vibeOS took would change meaning under a binary built for a later Linux (SYSCALL.md §8).

`- [ ]` and `- [x]` are the live status. Edit them in the commit that lands the work. There is no third
state. A deferral is an open box with a trailing note naming the phase that lands it, and that phase's
gate cannot close while the box is open.

A box is ticked only by the commit that makes the test proving it pass. That commit names the test in a
`Proves:` trailer, one per box it ticks: a host, in-guest, or user test by name, an e2e marker, a
`make` target, or a `scripts/check_*.py` script. For a document, name the script that checks the
document; for a deletion, name the check that finds nothing left. A box whose proof cannot be named is
not done. The kernel review found that ticking a box for work that had not landed was the most common
failure in this tree (KERNEL_REVIEW.md §5 and §8.3).

**Slices.** A phase lands as two to four PRs named A, B, C, D, each with its own in-guest tests and each
leaving `main` green. The last slice closes the gate, and its commit gets the `phase-<N>` tag and the
next release (see the standing gates). Phase 10 is the exception. It lands as one PR per review-issue
code or small group of codes, named by those codes (for example `B1+DX1`), and its lines without a code
land as PRs named after their subsection, in the three waves its preamble orders.

**Stretch** subsections, the [Beyond](#beyond) list, and [Funded goals](#funded-goals) are excluded from
exit gates. They are where the hard, optional, or paid things go, so that a phase is either done or not.

**Exit gate** is the definition of done. Gates are verifiable from outside the code: a marker appears
in serial output, a test target passes, a command produces the right result. "The code is written" is
not a gate. If a gate cannot be checked by running something, it is written wrong. A document counts as a
gate only when a script in `make check` verifies its required parts.

A gate that measures time, a rate, or throughput, or sizes its test against memory, names the
conditions it holds under: the accelerator, guest memory, and CPU count, and the VMM when it is not QEMU.
A gate that names none holds in the default harness guest (the `VIBEOS_*` defaults: TCG, 128 MiB, 2
CPUs), unless its era preamble names a default guest and hosts of its own, as Era VII's does. "Under
KVM" means KVM on a hosted x86_64 runner, where the §10.1 KVM leg runs, and HVF on the arm64 dev host,
as a §10.9 record, since hosted arm64 runners have no KVM. The hosted runner's CPU model changes from job
to job, so a fixed threshold under KVM holds on every model the runner draws, unless the gate says its
threshold is per model (§10.1).

A line in phase *N* never depends on work in a later phase. When it would, the work moves earlier or the
line moves later. A pointer to a later phase is only a cross-reference, such as "USB HID arrives through
§20.3". A deferral box is the one exception: it stays open in its own phase and blocks only the gate of
the phase it names. An exit-gate line is never deferred past its own phase's tag, though: a phase's gate
closes only when every one of its gate lines is checked, reopened lines included, so a gate line the
kernel review reopened blocks both its own phase's tag and the gate of the phase its note names.

**Free by default.** The project has no budget: the maintainer spends nothing on vibeOS beyond their own
agent tokens. Every phase, box, gate, and [Beyond](#beyond) entry is provable on these free resources:

- GitHub-hosted standard runners, free and unlimited for this public repository: x86_64 `ubuntu-24.04` and `ubuntu-26.04` (4 vCPUs, 16 GB, 14 GB of disk) with `/dev/kvm` once a udev rule opens it to the runner user; arm64 `ubuntu-24.04-arm` and `ubuntu-26.04-arm` (4 vCPUs, 16 GB, 14 GB of disk) with no KVM, so aarch64 guests there run under TCG; and macOS arm64 runners with no Hypervisor.framework. None has a GPU. A job runs at most 6 hours, and at most 20 jobs run at once, 5 of them macOS
- QEMU under TCG everywhere, KVM on the x86_64 runners, and HVF on the dev host, with its models of real devices: NVMe with subsystems, SR-IOV, and ZNS; AHCI; xHCI with USB HID, storage, network, and audio devices; e1000, e1000e, rtl8139, and igb with 8 SR-IOV VFs; HDA and virtio-sound, playing back to a WAV file; SD and eMMC; `intel-iommu`, `virtio-iommu`, and SMMUv3; a TPM through `swtpm`; the i6300esb and ICH9 TCO watchdogs, `pvpanic`, and ERST; virtio-gpu with up to 16 heads and EDID; PCIe hotplug; S3 on `q35`; machine-check injection on x86_64 and GHES error injection on aarch64. Under TCG it also runs guests larger than any runner, up to 4096 vCPUs in x2APIC mode on `q35` and 512 with GICv3 on `virt`, with memory the host commits only as the guest touches it, and gives aarch64 guests on `virt` EL2, FEAT_NV2, and an emulated PMUv3
- free VMMs and mocks on the x86_64 KVM runner: Firecracker with MMDS, cloud-hypervisor, QEMU's `microvm`, Microsoft's OpenVMM with its VMBus devices and MANA, EC2 and GCE metadata mocks, and cloud-init's NoCloud. No free, licensed model of AWS's ENA or Google's gVNIC exists
- free corpora and suites: the ACPI tables of 815 real machines from linuxhw/ACPI, recompiled with `iasl`; ACPICA's `aslts`; `fwts`; Linux's device-tree sources; LTP and kselftest; BlueZ's testers with its `btdev` controller; `v4l2-compliance`; Mesa's `llvmpipe` and `lavapipe` with dEQP and piglit; and `hostapd` and `wpa_supplicant` over the `mac80211_hwsim` virtio protocol with `wmediumd`
- the maintainer's Apple Silicon Mac (M4 Pro, 48 GB) as a VM host, never as a bare-metal test machine: aarch64 guests under HVF, with EL2 for guests from QEMU 11.1, and x86_64 guests under TCG only. Its runs are §10.9 dev-host records. No line installs a system service, or anything that runs as root, on it: a change to the owner's machine is the owner's decision (DESIGN §2.10)

A gate never needs a physical machine, a rented one, a paid service, or a new account. Free cloud tiers
need the maintainer's card and account, so they count as paid. What money or a spare machine would add
is in [Funded goals](#funded-goals), each entry with a rough cost and the lines it would add, and nothing
in a phase depends on one. A funded goal moves into a phase by an edit to this file once the machine or
the money exists.

Long runs, nested virtualization, and comparisons with Linux follow from those limits. A hosted job
lasts at most 6 hours, so a longer run is sharded into jobs of at most 5.5 hours that carry their state
forward as workflow artifacts, and its gate says so; unsharded uptime counted in weeks is a funded goal.
Nested virtualization on the hosted x86_64 runners works, but GitHub calls it experimental, and each job
gets AMD SVM or Intel VMX at random: a gate on it tests the path its job's CPU offers and needs a green
run of each vendor's path that §20.8's records show a runner offering within the last 7 nightly runs. aarch64 hypervisor gates run under TCG with
`virtualization=on`, with FEAT_NV2 when a guest hypervisor nests, and add a dev-host record under HVF
with QEMU 11.1 or later. A comparison with Linux boots Linux in the same guest shape on the same runner,
in the same job where it fits, so runner noise cancels.

**Standing gates** apply to every phase and are not repeated:

- `make` builds with `-D warnings`, the `x86_64-unknown-none` kernel build included (F147)
- `make check` green (fast local gate)
- `make test` green, all tiers, including the SMP and timer fallback variants once they exist, and on both architectures once Phase 11 lands
- CI green: the per-push jobs, and the scheduled jobs in the §10.1 CI budget before a phase tag
- new serial markers registered in the contract in [DESIGN.md](DESIGN.md#83-end-to-end), same commit
- new portable logic has host unit tests; new hardware behavior has an in-guest test
- from §13.9's uids on, a line that adds an interface an unprivileged process can reach (a syscall or flag, an `ioctl`, a device node, a `/proc`, `/sys`, tracefs, or debugfs file, a netlink message, or a socket option) names the check Linux makes on it (a capability, a uid, a file mode, or an rlimit) and the bound Linux puts on the kernel memory or CPU time one user can take through it (an rlimit, a sysctl such as `fs.epoll.max_user_watches`, or cgroup accounting), or says Linux has none, and adds an in-guest case, run as uid 1000, that is refused or stopped at that bound; §18.6's audit then covers it
- every fixed bug gets a regression test in the cheapest tier that catches it
- `CHANGELOG.md` entry for anything visible to someone running the kernel (≤ 2 lines, user-facing)
- from Phase 8 on, the commit that closes a phase's gate gets an annotated `phase-<N>` tag and the next release, `v0.<m>.0`, where *m* is one more than the last release's. The maintainer pushes both tags and then dispatches `release.yml` from `main` with the release tag (§10.1). Phases 8 to 14 close in order, so their releases are `v0.8.0` to `v0.14.0`; a phase in that range is tagged only after the phase before it, so Phases 8 and 9, whose gate lines the kernel review reopened into Phase 10's sections, are tagged in order when those lines close, possibly on the commit that closes Phase 10. From Phase 15 on, phases close side by side, *m* follows closing order, and the release notes name the phase. Phase 39's release is `v1.0.0`; a phase that closes after it cuts the next `v1.<m>.0` (§39.3)
- design docs updated in the same commit as any change to an invariant or a constant
- no `TODO` describing a correctness gap. Those become lines in this file.
- every box ticked names its proof in a `Proves:` trailer (see above); from §10.9's `check_ticks.py` on, CI enforces it on every pull request
- every `unsafe fn` has a `# Safety` section and every `unsafe` block a one-line `// SAFETY:` reason; clippy's `missing_safety_doc` (with `check-private-items`) and `undocumented_unsafe_blocks` are denied from §10.1 on
- from Phase 10 on, a phase is tagged only when `make gate PHASE=N` passes; that command fails for any gate line but the tag line that has no §10.9 gate-map entry naming what proves it: a command, a CI job on GitHub-hosted runners, or, for a line or the part of one that runs under HVF, a record of its run on the Apple Silicon dev host, since no hosted CI runner can run an HVF guest
- from §11.7's ordering check on, every atomic ordering other than `SeqCst` outside test code, fences included, carries a one-line comment naming the access it pairs with, or saying it pairs with none; `scripts/check_orderings.py` in `make check` fails on one without

## Non-goals

Stated so nobody spends a week on them.

- POSIX certification. Compatibility is a means to running real software, not a goal.
- Microkernel architecture. Monolithic, deliberately.
- Loadable kernel modules. One image, drivers in-tree. Signing and KASLR cover a single ELF.
- Windows or macOS binary compatibility.
- CPU hotplug. Offlining a core for power management is not hotplug; that is §19.6.
- 32-bit x86. Long mode only.
- 32-bit user programs: ia32 and x32 on x86_64, AArch32 on aarch64. Linux compatibility covers 64-bit (LP64) binaries only.
- Being a good first kernel to read. Other projects do that better and on purpose.

## The arc

| Era | Phase | | Unlocks |
|-----|-------|---|---------|
| **I. Metal** | 0 | [Ignition](#phase-0-ignition) | A booting, testable, CI-gated tree |
| | 1 | [Memory](#phase-1-memory) | `alloc`, own address space |
| | 2 | [Traps, ACPI, Time](#phase-2-traps-acpi-and-time) | Interrupts, a clock, hardware discovery |
| | 3 | [Threads](#phase-3-threads-and-scheduling) | Concurrency, preemption |
| | 4 | [SMP](#phase-4-smp) | All cores, per-CPU everything |
| **II. System** | 5 | [Console and Log](#phase-5-console-input-and-logging) | Interactive, observable |
| | 6 | [Devices](#phase-6-device-model-and-buses) | PCIe, MSI, DMA, virtio |
| | 7 | [Block storage](#phase-7-block-storage) | Persistent bytes |
| | 8 | [Filesystems](#phase-8-filesystems) | Files, paths, mounts |
| | 9 | [User mode](#phase-9-user-mode-and-processes) | Ring 3, syscalls, processes |
| | 10 | [Consolidation](#phase-10-consolidation) | Portable core, user runtime, real tables |
| | 11 | [Portability](#phase-11-portability) | aarch64 first class, native runs on the dev host |
| | 12 | [Fault-driven memory](#phase-12-fault-driven-memory) | Demand paging, COW, mmap |
| | 13 | [Threads and IPC](#phase-13-threads-ipc-signals-and-the-posix-surface) | clone, pipes, sockets, futex, TTY, signals |
| **III. Platform** | 14 | [Userspace](#phase-14-userspace) | libc, init, coreutils, real shell, crypto |
| | 15 | [Network](#phase-15-networking) | TCP/IP, sockets, DNS, TLS |
| | 16 | [Graphics](#phase-16-graphics-and-windowing) | Compositor, windows, terminal |
| | 17 | [Self-hosting](#phase-17-self-hosting-toolchain) | vibeOS compiles vibeOS, on both architectures |
| **IV. Frontier** | 18 | [Hardening](#phase-18-hardening) | KASLR, W^X, sandboxing, fuzzing |
| | 19 | [Performance](#phase-19-performance-and-observability) | Tracing, RCU, tickless, NUMA, slab, reclaim |
| | 20 | [Hardware models](#phase-20-hardware-models) | Real-device drivers on QEMU's models, AML on 815 machines' tables, S3, hotplug, EFI variables |
| | 21 | [Virtualization](#phase-21-virtualization) | Hypervisor, containers |
| | 22 | [Distribution](#phase-22-distribution) | Installer, releases, CI that runs on vibeOS |
| **V. Ecosystem** | 23 | [Linux compatibility](#phase-23-linux-compatibility) | glibc and Debian, language runtimes, LTP |
| | 24 | [Source bootstrap and ports](#phase-24-source-bootstrap-and-ports) | Toolchains built from source, a ports tree |
| **VI. Production** | 25 | [Reliability](#phase-25-reliability) | Injected machine checks, crash dumps, watchdogs, persistent logs |
| | 26 | [Cloud-ready images](#phase-26-cloud-ready-images) | Cloud images on Firecracker, cloud-hypervisor, OpenVMM, and metadata mocks |
| | 27 | [Scale-up](#phase-27-scale-up) | Up to 1,024 vCPUs and 1 TiB guests under TCG, huge pages |
| | 28 | [Network at scale](#phase-28-network-at-scale) | Multiqueue virtio-net with RSS, igb SR-IOV VFs, 100k connections |
| | 29 | [Storage at scale](#phase-29-storage-at-scale) | RAID, volumes, scrub, NVMe multipath and ZNS, IOPS against Linux |
| | 30 | [Operations](#phase-30-operations) | Server software unattended in sharded runs, metrics, live update |
| **VII. Daily Driver** | 31 | [Desktop platform](#phase-31-desktop-platform) | Suspend in a VM, multitouch input, runtime PM, device firmware, the Linux baseline |
| | 32 | [Displays](#phase-32-displays) | Multi-head KMS on virtio-gpu with EDID and hotplug, planes and CRCs, IGT against Linux |
| | 33 | [Graphics stack](#phase-33-graphics-stack) | Mesa's software GL and Vulkan on Linux's render interface |
| | 34 | [Audio and cameras](#phase-34-audio-and-cameras) | ALSA and PipeWire on HDA and virtio-sound, a virtual camera |
| | 35 | [Wireless](#phase-35-wireless) | Simulated Wi-Fi with WPA3-SAE, a virtual Bluetooth controller |
| | 36 | [Desktop session](#phase-36-desktop-session-and-applications) | Wayland session, GTK and Qt, a browser, a screen reader |
| | 37 | [Daily driver](#phase-37-daily-driver) | A scripted day in a VM, nightly, measured against Linux in the same VM |
| **VIII. Assurance** | 38 | [Verification](#phase-38-verification) | Proved allocator and page tables, checked protocols |
| | 39 | [Stability](#phase-39-stability) | 1.0, frozen interfaces, supported releases |
| | | [Beyond](#beyond) | Hard things with no gate |

Eras I and II are the ones with known answers, so they are where agent performance is measurable
against a clear correct result. Eras III and IV are where it stops being clear, which is the point.
Eras V to VIII are judged by things vibeOS does not control: other projects' test suites, Linux booted
in the same VM, other projects' VMMs, and a proof checker.

From Phase 15 on the numbers are a reading order: Phases 15 to 17 start from 14 side by side, 18 and
19 are independent of each other, Phase 23 runs beside Phases 18 to 22, Phase 27 can start beside 21
and 22, and Phase 38 beside everything from Phase 20 on. 1.0 (Phase 39) needs Phases 22 to 25, 30, and
38, and none of 26 to 29 or 31 to 37, which continue beside it and after it (§39.3). Otherwise a phase
assumes the gate of the one before it; era preambles name the exceptions, and Eras I and II have none.

---

# Era I. Metal

Power-on to a preemptively scheduled multiprocessor kernel. Every mistake made here is paid for
repeatedly, so the bar is higher than the feature count suggests.

## Phase 0: Ignition

**Goal.** A tree that builds, boots, prints, panics usefully, and fails CI when broken. No kernel
features. The entire value of this phase is that everything after it can be trusted.

**Unlocks.** Everything. Also sets the crate structure that determines what can be host-tested for the
rest of the project, which is the single most consequential decision in this phase.

**Exit gate**
- [x] `make` produces a hybrid BIOS + UEFI ISO
- [x] boots in QEMU under both `-bios` default and OVMF
- [x] serial shows `vibeOS: serial online` as its first line
- [x] a deliberate `panic!()` prints file, line, and message, then `vibeOS: panic: halted` (`make test-e2e-panic`); no tier observes the halt, since the `panic_exit` build exits through `isa-debug-exit` and the harness kills QEMU at that line without checking the exit status (F141)
- [x] `make test-e2e` asserts markers in order, fails fast on panic signatures, exits early on success
- [x] `make test-harness` passes: the harness has its own unit tests
- [x] CI runs the full ladder on push and pull request

### 0.1 Toolchain and target
- [x] `rust-toolchain.toml`: dated nightly, `rust-src`, `llvm-tools`, `targets = ["x86_64-unknown-none"]`
- [x] built-in `x86_64-unknown-none`: `code-model: kernel`, `disable-redzone`, `-mmx,-sse,+soft-float`; rustflags force frame pointers, static relocation, `-no-pie`, `-znorelro`
- [x] no RELRO (`-znorelro`); it conflicts with a non-PIE static kernel
- [x] `.cargo/config.toml`: default target `x86_64-unknown-none`; linker script is an absolute `-T` from `build.rs`
- [x] `Cargo.toml`: kernel target aborts; host tests unwind (`profile.dev`); `opt-level = 1` for dev
- [x] library and binary targets split from the first commit, not retrofitted
- [x] `kernel_tests` feature declared now, wired in phase 1

### 0.2 Linker and image layout
- [x] `linker.ld` places the image at `0xFFFF_FFFF_8000_0000`
- [x] page-align every section boundary so per-section permissions are possible
- [x] `.got` before `.bss` and inside the mapped range
- [x] export `__kernel_vma_start`, `__kernel_vma_end`, and per-section start/end symbols
- [x] `make layout` target: `llvm-objdump` section table plus `llvm-nm` for the exported symbols, so layout mistakes are visible without booting

### 0.3 Limine handshake
- [x] request statics in `.limine_requests` with the start and end markers, all `#[used]`
- [x] base revision verified before reading any other response, with its own marker
- [x] requests: framebuffer, memory map, HHDM, executable address, RSDP, all read once by `boot::capture`
- [x] a `BootInfo` struct captured once at entry; nothing else reads Limine statics
- [x] each null response produces a named halt, not an unwrap panic in a function with no context
- [x] `limine.conf` with a single entry, serial console enabled

### 0.4 Serial and panic
- [x] COM1 16550 init: 115200 8N1, FIFO enabled, DLAB dance
- [x] polled TX with a bounded THRE wait; drop the byte at the cap rather than spinning forever
- [x] polled RX on the data-ready bit  (landed with §5.3)
- [x] `fmt::Write` implementation with no allocation, usable before the heap exists
- [x] `marker!` (contract lines, never filtered) and `klog!` (through the §5.5 log ring) write to it; `print!` and `println!` were removed in E3 (#83)
- [x] `#[panic_handler]`: re-init the port from scratch, print location and message, `cli; hlt` loop
- [x] a `panic_test` build feature or shell command so the panic path is exercised, not assumed

### 0.5 Build system
- [x] `Makefile`: `all`, `run`, `clean`, plus the test targets
- [x] `CARGO_TARGET_DIR` pinned to `./target`
- [x] kernel prerequisites from a `find` over `src/`, never a hand-written list
- [x] ISO staging: kernel ELF, `limine.conf`, BIOS and UEFI Limine artifacts, `xorriso` hybrid image, `limine bios-install`
- [x] `setup.sh`: fetch the Limine binary branch, verify `qemu-system-x86_64`, `xorriso`, `nasm`, `python3`; never rewrite project files
- [x] `make run` uses `-smp 2` so the default loop is multiprocessor from day one

### 0.6 Test harness
- [x] Python, standard library only; `subprocess` with its own timeout, no GNU `timeout`
- [x] QEMU spawned with serial captured and a monitor socket
- [x] ordered marker assertion, each check individually named in output
- [x] panic and exception signature scan, fail immediately with the captured line
- [x] early `quit` through the monitor on success
- [x] `VIBEOS_SMP` and `VIBEOS_QEMU_CPU` overrides
- [x] harness helpers have unit tests under `make test-harness`, except the QEMU runners `run_qemu_and_check`, `run_qemu_until_exit`, and `run_qemu_console_input`; `run_e2e.py` matches markers through `run_qemu_and_check` for every `test-e2e*` target, while the marker-order tests exercise `check_markers_in_order`, which no runner calls (F141)
- [x] targets: `test-unit`, `test-harness`, `test-e2e`, `test`

### 0.7 CI
- [x] GitHub Actions on push and pull request, Linux runner
- [x] install `qemu-system-x86`, `nasm`, `xorriso`; bootstrap Limine
- [x] `cargo fmt --check` and `cargo clippy -- -D warnings`, with the kernel linted under `--all-features`, `kernel_tests`, and `vibefs_crash` but never the default features; `make` compiles the kernel without `-D warnings`, since the `[target.x86_64-unknown-none]` rustflags shadow `[build]`'s and CI sets no `RUSTFLAGS` (F147)
- [x] `check` job (`make check`, then an 87% line-coverage floor on `vibeos-core` from `cargo llvm-cov`) before the QEMU ladder
- [x] run host units, harness units, ISO build, e2e
- [x] cache the cargo registry and the Limine checkout so the loop stays fast

---

## Phase 1: Memory

**Goal.** Own the address space and get a working `alloc`. Also stand up in-guest testing, because
paging cannot be verified from the host.

**Unlocks.** Every data structure in the kernel. `Vec`, `Box`, `String`. Thread stacks.

**Exit gate**
- [x] `vibeOS: pmm: <n> free 4KiB frames`, `paging: cr3 ok`, `heap ok`, `kva: ready` in order
- [x] kernel runs entirely on its own PML4 with Limine's tables abandoned
- [x] `make test-kernel` passes with in-guest tests for map/unmap, NX enforcement, heap growth, and the stack guard page
- [x] host tests cover the buddy allocator including fragmentation and exhaustion
- [x] a shell-less `meminfo` dump on the boot log (`diag::meminfo`, the `vibeOS: meminfo:` lines); no test checks its totals

### 1.1 Buddy physical allocator
- [x] `Buddy` in the library half: `allocate(order)`, `deallocate(phys, order)`, intrusive free lists stored in the free pages
- [x] split on allocation, merge with the buddy on free, up to a max order covering at least 4 MiB
- [x] O(1) free frame counter, no list walking
- [x] `stats()`: total, free, largest available order
- [x] host tests: exhaustion returns `None`, random alloc/free returns the count to its initial value, per-order alignment, coalescing after freeing alternate blocks, double free detected
- [x] init from the Limine memory map, excluding frame 0, the kernel image, the framebuffer, `0x8000`, and everything not `USABLE`
- [x] design for a future per-frame metadata array; do not paint into a corner where refcounts cannot be added

### 1.2 Page tables
- [x] 4-level walk with typed abstractions for `PhysAddr`, `VirtAddr`, and PTE flags
- [x] `map_page`, `map_range`, `unmap_page`, `translate`, with 2 MiB page support
- [x] build a fresh PML4 from buddy frames: kernel image per section, physmap at the HHDM offset with 2 MiB pages, 512 MiB low identity with the first 2 MiB executable, bootloader stack window duplicated from Limine's tables
- [x] `map_end` derived from usable RAM, kernel image end, and framebuffer extent, capped at 8 GiB
- [x] set `EFER.NXE` before installing, then `mov cr3`
- [x] `invlpg` after every single-PTE edit
- [x] TLB shootdown hook present as a single-CPU no-op so phase 4 changes one function, not fifty call sites
- [x] assert on overlapping regions and on mapping over an existing present entry unless explicitly asked to remap

### 1.3 MMIO attributes
- [x] `patch_physmap_uc(phys, len)` setting PCD and PWT without splitting 2 MiB entries
- [x] `ioremap(phys, len)` reserving from the dedicated MMIO window for devices that should not be reached through the physmap
- [x] in-guest test: patch a page, verify the PTE flags read back

### 1.4 Kernel heap
- [x] free-list heap at `HEAP_START`, 1 MiB initial, growing in page increments to the 64 MiB region cap
- [x] backing pages from the buddy allocator, mapped writable and NX
- [x] `GlobalAlloc` wrapper disabling interrupts across `alloc` and `dealloc`
- [x] `#[alloc_error_handler]` panicking with the failed `Layout`
- [x] in-guest tests: `Box`, `Vec` growth past the initial mapping, alignment from 1 to 4096, allocate/free/reallocate reuse, OOM reaching the error handler rather than corrupting

### 1.5 Kernel VA allocator
- [x] range allocator over the 64 GiB KVA region, first fit, freed ranges to the tail of the free list
- [x] `alloc_guarded_stack(pages)`: reserve `pages + 1`, map the upper `pages` from separate order-0 frames, leave the bottom unmapped
- [x] `vmap(frames)` for non-contiguous physical memory presented contiguously
- [x] deferred free list for stacks that cannot be unmapped yet, with an explicit drain
- [x] assert the region is entirely unmapped before claiming it
- [x] in-guest tests: guard page write faults, alloc/free roundtrip restores the frame count, deferred drain actually frees

### 1.6 In-guest test infrastructure
- [x] `kernel_tests` feature builds a second kernel that runs a test registry after init
- [x] separate Cargo target directory and separate ISO, so a test build can never be packaged as production
- [x] `ktest_ok` / `ktest_fail` / `ktest_skip(name, reason)` with the serial protocol from [DESIGN.md](DESIGN.md#82-in-guest-tests)
- [x] `isa-debug-exit` at port `0xf4`: `0x10` for pass, `0x11` for fail
- [x] `tests/harness/run_ktest.py` requiring `begin` and `end`, rejecting any `FAIL`, checking the exit status
- [x] failing tests print enough context to diagnose without a rerun

### 1.7 Diagnostics
- [x] `meminfo`-style dump: total, free, used, largest order, heap used and capacity, KVA used
- [x] page table dump walker for debugging, printing ranges rather than individual entries

---

## Phase 2: Traps, ACPI, and Time

**Goal.** Interrupts that work, exceptions that explain themselves, hardware discovery through ACPI,
and a monotonic clock nobody has to distrust.

**Unlocks.** Preemption. Delays that AP bring-up needs. Every driver.

**Exit gate**
- [x] `gdt ok`, `pic: remapped`, `idt ok`, `acpi: xsdt <n> tables`, `time: tsc <n>/ms` in the order [DESIGN.md](DESIGN.md#33-_start-order) specifies
- [x] a kernel `int3` returns; `int3_roundtrip` does not check that the `#BP` handler ran (F142)
- [x] a deliberate `#GP` prints its interrupt frame (`rip`, `cs`, `rflags`, `rsp`, `ss`), the error code, `cr3`, and the handler's own `rbp` (F070), not the interrupted general-purpose registers, then `vibeOS: panic: halted` (`make test-e2e-gp`); no tier observes the halt (F141)
- [x] a deliberate stack overflow lands in the double fault handler on its IST stack, proven by an in-guest test
- [x] `pit_tick_rate` sees `uptime_ms` advance 40 to 160 ms over an 80 ms TSC busy-wait; in the in-guest tiers the LAPIC periodic timer drives that tick, and the PIT drives it only in `make test-e2e-pit`, which measures no rate
- [x] TSC calibrated against the HPET when present, PIT channel 2 otherwise, both paths tested
- [x] `now_us` monotonic across 10k reads with a timer firing underneath (`now_us_monotonic`, `now_us_under_yields`), by construction: `now_ns` returns `LAST_NS.fetch_max(n).max(n)`, so neither test can fail, and the second calls `hlt` every 200 reads instead of yielding (F100)
- [x] host tests: RSDP v1 and v2 checksum rejection, HPET table validation, MADT iteration over truncated input, `now_us` seqlock retry under a simulated concurrent writer

### 2.1 GDT, TSS, IST
- [x] flat GDT: null, kernel code, kernel data, user code, user data, TSS
- [x] user selectors placed in the order `syscall`/`sysret` requires, before ring 3 exists
- [x] TSS with `RSP0` and the IST array
- [x] IST stacks page-aligned and per-CPU: 1 double fault, 2 NMI, 3 machine check, 4 debug
- [x] structure it so per-CPU GDT and TSS instances are natural, since phase 4 needs them

### 2.2 IDT and exceptions
- [x] all 256 entries populated; unhandled vectors get a diagnostic default rather than a reserved gate
- [x] `x86-interrupt` ABI handlers
- [x] halting handlers dump RIP, CS, RFLAGS, RSP, SS, the error code, and CR2 for faults
- [x] `#BP` logs and returns; `#UD`, `#GP`, `#PF`, `#DF` log and halt; the `#MC` handler is installed on IST 3 but cannot run, since no CPU sets `CR4.MCE`, so a machine check shuts the CPU down (F026)
- [x] a scoped transient fault handler for tests: install, run a faulting operation, step RIP past it, restore
- [x] named vector constants in `vectors.rs`, with a host test asserting that the entries of `NAMED` are unique; `NAMED` omits `MC` (F093)
- [x] in-guest tests: `int3` roundtrip, double fault on IST via stack overflow, the scoped handler catching a deliberate `#PF`

### 2.3 8259 PIC
- [x] remap master to `0x20`, slave to `0x28`, ICW sequence with `io_wait` between writes
- [x] mask everything immediately after remap
- [x] `unmask(irq)` / `mask(irq)` / `disable_all()`
- [x] remap and mask before the first `sti` on every boot: the ICW sequence before `lidt` is skipped when FADT `IAPC_BOOT_ARCH` bit 0 is clear, and a second one after TSC calibration always runs, because bit 0 is `LEGACY_DEVICES`, not 8259 presence (QEMU clears it and has an 8259). §20.1 decides presence from the MADT
- [x] spurious IRQ7 and IRQ15 handled without a bogus EOI

### 2.4 ACPI
- [x] RSDP validation: signature, v1 checksum over 20 bytes, v2 extended checksum over the full length
- [x] XSDT walk with per-table checksum validation, RSDT fallback
- [x] all packed field access through `read_unaligned`
- [x] MADT: LAPIC base, type 5 address override, I/O APIC entries with GSI bases, type 2 interrupt source overrides, enabled processor APIC IDs
- [x] HPET: main counter base and period, rejecting zero addresses and I/O-space generic address structures
- [x] FADT: `IAPC_BOOT_ARCH`, `RESET_REG` and `RESET_VALUE`, and the HW-reduced `SLEEP_CONTROL_REG` and `SLEEP_STATUS_REG`; the PM1a and PM1b control blocks and the `Flags` field are not parsed (F097)
- [x] MCFG: PCIe ECAM base, stored for phase 6
- [x] parsing lives in the library half against synthetic table bytes, so all of it is host-tested
- [x] MMIO for every discovered table patched uncacheable before first access
- [x] boot log summarizing what was found: table count, CPU count, I/O APIC count, HPET presence

### 2.5 PIT and the bootstrap tick
- [x] channel 0, mode 2, divisor 1193 for ~1 kHz on IRQ0
- [x] `io_wait` between the low and high divisor byte writes
- [x] handler: increment the tick counter, snapshot the TSC, EOI, return. No allocation, no logging.
- [x] channel 2 one-shot for calibration, gated through port `0x61`

### 2.6 TSC calibration
- [x] check the invariant TSC CPUID bit and log loudly if absent
- [x] `lfence` before `rdtsc`, or use `rdtscp`
- [x] calibrate against the HPET main counter over ~10 ms when available
- [x] fall back to PIT channel 2 with a count of 11932
- [x] `tsc_per_ms` calibrated once on the BSP and read by every CPU through `time_init::tsc_per_ms()`; `PerCpu.tsc_per_ms` holds a copy that only an in-guest test reads (F027, F111)
- [x] sanity-check the result against a plausible range and refuse a value that would poison every delay downstream
- [x] `busy_wait_ms` on the TSC, using `hlt` when interrupts are enabled

### 2.7 Timekeeping
- [x] tick counter and TSC snapshot published under a seqlock: odd bump (`fetch_add(AcqRel)`), `Relaxed` stores of both, even bump (`Release`); no release fence follows the odd bump, so only x86's locked `xadd` keeps a reader from pairing a new payload with an unchanged sequence (F098)
- [x] reader retries until it sees a stable even sequence, with acquire ordering
- [x] `uptime_ms`, `now_us`, `now_ns` built on it
- [x] the interpolation arithmetic lives in the library half and is host-tested including near `u64::MAX`
- [x] the host test `seqlock_threaded_writer_never_tears` writes each tick and TSC pair with a fixed relation between them, so a torn read fails it; no in-guest test can see a tear (F100)
- [x] `time::next_deadline(instant)` defined and host-tested (`next_deadline_is_1ms_ahead`); no timer path calls it, and every timer mode runs a fixed 1 ms period (tickless idle is §19.6)
- [x] RTC read once at boot for wall clock, tracked forward with the monotonic clock plus an offset

### 2.8 Diagnostics
- [x] `uptime` reporting tick milliseconds and TSC microseconds side by side, so divergence is visible
- [x] a boot line naming the calibration source and the measured frequency

---

## Phase 3: Threads and Scheduling

**Goal.** More than one thing running. Preemptive round-robin over kernel threads with real
synchronization primitives.

**Unlocks.** Blocking drivers. Anything that waits. Processes, eventually.

**Exit gate**
- [x] `sched: cpu0 ready` marker, and `irq: enabled` after it
- [x] timer preemption runs a spawned CPU-bound thread on CPU 0 while the bootstrap thread spins there without yielding (`preempt_two_threads` at `-smp 2`); at `-smp 4` both workers can land on idle APs, and the test then passes with no preemption (F142)
- [x] `sleep_ms(50)` returns within 50 to 100 ms of `uptime_ms` on an invariant TSC; without one, as under TCG, `sleep_ms_50` accepts 40 to 400 ms of `now_us`, because timer ticks coalesce (F027)
- [x] in-guest: 1000 iterations of two threads contending a blocking mutex, no deadlock, correct final count
- [x] a thread that returns is reaped and its stack returned to the allocator, proven by the frame count
- [x] the idle thread runs when nothing else is ready and the system does not wedge
- [x] host tests for the run queue state machine and the timeout ordering structure

### 3.1 Thread abstraction
- [x] `ThreadId`, `Tcb` with state, kernel stack handle, saved context, entry point, name
- [x] states: ready, running, sleeping with deadline, blocked on a wait queue, dead
- [x] guarded 16 KiB kernel stack from the KVA allocator for every spawned thread; the bootstrap thread, which runs boot and, in `kernel_tests` builds, `ktest::run`, stays on Limine's boot stack (at least 64 KiB, no guard page) (F072)
- [x] `spawn(name, fn)` returning a handle
- [x] a global TCB table so a thread is addressable by id from anywhere, laid out for per-CPU queues in phase 4

### 3.2 Context switch
- [x] `switch_context(old: *mut CpuContext, new: *const CpuContext)` in `global_asm!`
- [x] save and restore callee-saved GPRs, `rflags`, `rsp`, and the return address
- [x] new threads start on a synthetic frame that returns into the trampoline that calls the entry point
- [x] a thread returning from its entry point marks itself dead and schedules, never falls off the stack
- [x] `switch_context` saves no FPU or SSE state; since §9.1, `syscall_init::on_switch` saves and restores each TCB's FXSAVE area (`switch_fpu`) before calling it

### 3.3 Scheduler
- [x] ready queue, sleep queue ordered by deadline, per-wait-queue blocked lists (`WaitQueue`)
- [x] `schedule()` for the voluntary path
- [x] `on_timer_tick()` called after EOI, preempting every 10 ticks
- [x] `yield_now()`
- [ ] a dead thread's kernel stack is freed only after its own CPU has switched off it, never by another CPU's `reap_zombies` while the exiting CPU still runs on it. Reopened by the kernel review (F012); lands in §10.10.
- [x] every scheduler lock acquisition inside an interrupt guard, no exceptions
- [x] context switch counter and per-thread run time accounting from the start; retrofitting instrumentation is worse than building it in

### 3.4 Sleep and timeouts
- [x] `thread::sleep_ms` parks on the sleep queue and wakes from the timer path
- [x] one timeout structure: sorted list under a lock initially, with the interface a timing wheel can replace
- [x] every blocking primitive except `thread_init::wait_on` and `block_init::IoWaiter::wait` takes an optional deadline; those two, and any primitive given a `None` deadline, park until `FAR_DEADLINE` (`u64::MAX` ns), so a thread whose wake is lost stays blocked (F016, F033, F034)
- [ ] a periodic sweep that logs each thread still blocked `OVERDUE_NS` (5 s) past its deadline. Reopened by the kernel review (F111); lands in §10.7.

### 3.5 Synchronization
- [x] `InterruptGuard`: save `RFLAGS.IF`, `cli`, restore on drop, correctly nested
- [x] one `SpinMutex`: CAS acquire, IRQ-aware, `assert!` not `debug_assert!` on misuse
- [x] `BlockingMutex` over a wait queue, with the enqueue then mark then drop then schedule ordering
- [x] `RwLock`, `Semaphore`, `Condvar`
- [x] `Channel<T>` bounded MPSC for producer/consumer between threads
- [x] `WaitQueue` as the shared primitive underneath all of them
- [x] host tests for whatever is testable without hardware; in-guest tests for contention

### 3.6 Idle
- [x] one idle thread, always runnable, lowest priority, `sti; hlt`
- [x] idle time accounted so a load figure is possible later
- [x] structured for one idle thread per CPU in phase 4

### 3.7 Verification
- [x] in-guest: spawn writes a sentinel; N threads increment a shared counter under the blocking mutex; `sleep_ms` accuracy; `yield_now` actually switches; the idle thread runs
- [x] hold the scheduler lock with IF off, assert ticks freeze (cannot `int $0x20` while holding: recursive SCHED), then force a timer IRQ after drop and check nest. Regression for a lock taken with interrupts enabled.
- [x] stress: spawn and exit thousands of threads, assert the frame count returns to baseline

---

## Phase 4: SMP

**Goal.** Every core running, every core scheduling, and the locking correct enough that it stays up
under load.

**Unlocks.** Real parallelism. Also every latent locking bug in phases 1 through 3, which is why this
comes before drivers rather than after.

**Exit gate**
- [x] `smp: done` before `shell ready`, with at least `N-1` `smp: ap online` lines before it, in order, at `-smp N`; the harness matches an ordered subsequence, so an extra line passes (F141)
- [x] `sched: cpu<i> ready` for every CPU
- [x] `time: lapic_timer ok (<mode>)` naming the mode that was selected
- [x] `make test-kernel` at `-smp 2` and `-smp 4` both pass, with the harness retrying a timed-out boot and the known failures §10.2 lists (F011, F021, F074)
- [x] `make test-lapic-fallback` passes with `-cpu qemu64,-tsc-deadline`; under TCG, which never offers TSC-deadline, it takes the same periodic LAPIC path as `make test-kernel` (F078)
- [x] a thread spawned on CPU 0 observably runs on another CPU
- [x] remote unmap and remap through the shootdown path passes in-guest with 2+ CPUs
- [x] failed AP bring-up frees everything it allocated, verified by frame count with a fault injected

### 4.1 LAPIC
- [x] enable via `IA32_APIC_BASE` bit 11, honoring the MADT type 5 address override
- [x] spurious vector register set to `0xFF` with the enable bit
- [x] TPR set to 0
- [x] EOI helper, and dispatch that knows PIC-routed from APIC-routed vectors
- [x] `send_ipi(dest, vector, mode)` with a bounded delivery-pending poll
- [x] the poll loop in the library half with host tests for both the clears and the timeout case
- [x] LVT setup for error and thermal, so a LAPIC error is reported rather than silent

### 4.2 I/O APIC
- [x] enumerate every I/O APIC from the MADT with its GSI base
- [x] indirect register access through `IOREGSEL` and `IOWIN`
- [x] `route_gsi(gsi, vector, cpu, trigger, polarity)` writing the high dword before the low
- [x] apply interrupt source overrides; never assume ISA IRQ *n* is GSI *n*
- [x] `mask_gsi` / `unmask_gsi`
- [x] mask the PIC completely once routing is live and the LAPIC timer is verified

### 4.3 LAPIC timer
- [x] detect TSC-deadline via `CPUID.01H:ECX[24]`; LVT mode `10b`, arm `IA32_TSC_DEADLINE`; the arming path has run in no tier, since TCG never sets the bit (F078)
- [x] fall back to periodic mode, calibrating LAPIC ticks per millisecond against the HPET with divider 16
- [x] fall back to the PIT with a single global tick and no per-CPU preemption
- [x] rearm before calling the scheduler
- [x] mask the PIT's GSI when the LAPIC owns the tick
- [x] log which mode was chosen, and make it an e2e assertion so a silent downgrade is not invisible
- [x] in-guest tests: the timer fires, and the periodic timer keeps firing across many ticks (`lapic_timer_rearm`); `rearm_deadline`, the only software rearm, returns before its `IA32_TSC_DEADLINE` write in every tier (F078)

### 4.4 AP trampoline
- [x] `trampoline.S` via `global_asm!` into `.trampoline`, copied to `0x8000`; blob fits below the param block
- [x] real mode to protected mode to long mode, setting `EFER.LME` and `EFER.NXE`
- [x] parameter block at the documented offsets: CR3, stack top, entry point, IDT pointer
- [x] each parameter written with `write_volatile`, `compiler_fence(SeqCst)` before the SIPI
- [x] `0x8000` identity mapped, executable, and permanently excluded from the PMM
- [x] a static assertion that the blob fits below its parameter block

### 4.5 AP bring-up
- [x] one AP at a time, INIT, 10 ms, SIPI, ~1 ms, SIPI
- [x] per-CPU area and guarded stack allocated and published with a fence before the SIPI
- [x] 3 second ready-flag timeout; on failure free the stack and per-CPU area, log, and continue; the free is unsafe for an AP that accepted the SIPI and then stalled, and §20.1 replaces it with INIT and a leak (F032)
- [x] AP path in order: per-CPU GDT and TSS, per-CPU MSRs (`GS_BASE` before any `lidt`), IDT, LAPIC enable, timer armed with the BSP's calibration (`apic_init::arm_ap`), ready flag, `sti`, enter as idle (F027)
- [x] `GS_BASE` set before the IDT is live and before `sti`
- [x] an online mask, and a barrier the BSP waits on before declaring `smp: done`

### 4.6 Per-CPU data
- [x] `PerCpu` with `self_ptr` at offset 0, reached through `GS_BASE`
- [x] `KERNEL_GS_BASE` set to the same value at bring-up; since §9.1, `swapgs` exchanges it with the user GS base, and syscall entry saves the user RSP in `syscall_scratch` (F148)
- [x] heap-allocated array sized from the actual MADT CPU count
- [ ] `per_cpu!` accessors safe from interrupt context, including a device-pool or keyboard interrupt taken at CPL 3. Reopened by the kernel review (F004); lands in §10.6.
- [x] contents: cpu id, APIC id, ready queue, wake inbox, idle handle, current thread, local ticks, context switches, `tsc_per_ms`, timer mode
- [x] in-guest identity tests on both BSP and AP

### 4.7 Locking audit
- [x] every lock taken from an ISR audited for interrupt-disabled acquisition in all contexts
- [x] the documented lock rank order enforced by review and by the per-CPU `HELD` rank tracker in `sync_init.rs`, in every build; the tracker misses a spinlock held across a switch, two locks of one rank nested, and the unranked `IrqCell`s other CPUs take (F108)
- [x] serial TX locked at byte granularity
- [x] remote unmap through the shootdown path (`tlb_shootdown_remote`) passes in-guest
- [ ] the scheduler is SMP-safe; today another CPU can unmap a dying thread's stack before its CPU switches off it, and other CPUs read `PerCpu` fields while the owner CPU writes them. Reopened by the kernel review (F012, F039); lands in §10.3 and §10.10.
- [x] a stress test hammering the allocator from every CPU simultaneously

### 4.8 Per-CPU scheduling
- [x] per-CPU ready queues, global TCB table
- [x] `CpuAffinity::{Any, Pinned(cpu)}`
- [x] round-robin placement for `Any`
- [x] cross-CPU wake through the target's inbox plus a reschedule IPI, never a remote queue lock
- [x] one idle thread per CPU with its own stack
- [x] global sleep queue under one lock for now
- [x] when locking two CPU-local structures, lower `cpu_id` first
- [x] in-guest: cross-CPU spawn roundtrip, reschedule IPI delivery, waking an idle AP

### 4.9 IPIs
- [x] `0xFD` reschedule
- [x] `0xFC` TLB shootdown
- [x] `0xFB` call-function, with a wait-for-completion variant
- [x] `0xFE` panic halt broadcast (Fixed delivery); `halt_others` does not wait for acknowledgement, and a CPU spinning with IF=0 does not take it, so that CPU can still write COM1 during the dump (F135)
- [x] handlers allocation-free; shootdown/call/halt take neither PT nor SCHED; reschedule takes SCHED IRQ-off

### 4.10 TLB shootdown
- [x] update the PTE, broadcast, and wait for acknowledgement from every online CPU; `wait_acks` panics after 1000 × `tsc_per_ms` cycles (about 1 s) without every ack (F011)
- [x] the waiting initiator keeps IF off and services incoming shootdown requests so two simultaneous shootdowns cannot deadlock
- [x] KVA free deferred until the shootdown completes; freed ranges to the tail of the free list
- [x] in-guest: unmap on one CPU, verify a fault on another, remap, verify access

### 4.11 CI variants
- [x] `-smp 2` as the default everywhere including e2e
- [x] `-smp 4` target
- [x] `-cpu qemu64,-tsc-deadline` target
- [x] the `-smp 4` in-guest tier re-run weekly by `smp-stress.yml` (`make test-smp-stress`, 180 s timeout); it adds no load over `make test-kernel-smp4` and applies the same retries (F021)
- [x] `cpus` diagnostic output: logical and APIC id, online mask, timer mode, local ticks, ready depth, context switches

---

# Era II. System

From a kernel that schedules to an operating system that runs programs against files, then the same
operating system on a second architecture, then the memory and process semantics real software assumes.

## Phase 5: Console, Input, and Logging

**Goal.** Interactive from the QEMU window, and observable enough to debug the phases that follow.

**Unlocks.** Manual exploration. `dmesg`. A shell to hang commands off.

**Exit gate**
- [x] `console ok` and `shell ready` markers
- [x] typing in the QEMU window while the shell waits for input echoes on the framebuffer and drives commands (F004)
- [x] `dmesg` shows the full boot log including lines emitted before the framebuffer existed
- [ ] a panic on any CPU stops the others and leaves a readable dump with a symbolized backtrace. Reopened by the kernel review (F070, F084, F135); lands in §10.2 and §10.7.
- [x] host tests: scan code decoding, ring buffer wrap, line editing, command tokenization
- [x] log level filtering changeable at runtime and visible in output

### 5.1 Framebuffer console
- [x] BGRX pixel writes at `base + y * pitch + x * 4`, with bounds checks that are not `debug_assert`
- [x] 8x8 bitmap font, LSB leftmost, ASCII 32 to 126, with a defined glyph for everything else
- [x] text grid with wrap and scroll; scroll by `memmove` of whole rows
- [x] a banner region that survives scrolling
- [x] host tests for the font table and for the pixel bounds arithmetic
- [ ] double buffering off the physmap, so scrolling stops tearing (lands in §16.1, where the display abstraction owns the scanout buffer; today the console scrolls the framebuffer in place)

### 5.2 PS/2 keyboard
- [x] controller init, self test, and enabling the first port
- [x] scan code set 1 decoding including `0xE0` prefixes
- [x] modifier state: shift, ctrl, alt, caps lock, num lock
- [x] `DecodedKey` as either a character or a named non-printing key
- [x] fixed-size ring buffer written by the ISR, drained by the consumer with interrupts disabled
- [x] the decoder and ring buffer in the library half, fully host-tested
- [x] ISR does no allocation and no logging

### 5.3 Console multiplexer
- [x] `ConsoleBackend` trait: write, and optional read
- [x] serial and framebuffer backends registered, output fanned out to both
- [x] input merged from the PS/2 ring and from serial RX
- [x] one lock discipline that does not deadlock against the keyboard ISR
- [x] runtime enable and disable per backend

### 5.4 Shell
- [x] line editor: echo, backspace, ctrl+C, ctrl+U, cursor movement, history
- [x] tokenizer handling quotes and consistent whitespace behavior, in the library half with host tests
- [x] built-ins: `help`, `echo`, `meminfo`, `uptime`, `cpus`, `dmesg`, `ps`, `panic`, `reboot`, `poweroff`
- [x] a command table that new subsystems register into rather than a growing match arm
- [x] running as a kernel thread, not in `_start`, so it can block

### 5.5 Kernel log
- [x] levels: error, warn, info, debug, trace, with compile-time and runtime filtering
- [x] a lock-free-enough ring buffer that survives a panic and is readable afterward
- [x] every record timestamped from the monotonic clock and tagged with its CPU
- [x] boot lines captured into the ring before the framebuffer exists (replay when FB lands in B)
- [x] `dmesg` with level filtering and follow (`dmesg -f`; Ctrl+C to stop)
- [ ] a per-CPU log buffer drained by a printer thread (lands in §19.5; today one global IRQ-safe ring and a serial try-lock sink); whole-line serial output lands earlier, in §10.2 (F138)

### 5.6 Panic and diagnostics
- [ ] broadcast the halt IPI first so other cores stop before the log is written. Reopened by the kernel review (F135); lands in §10.7.
- [ ] symbolized backtrace: keep a symbol table in the image, walk frame pointers. Reopened by the kernel review (F070, F084, F139); lands in §10.2 and §10.7.
- [x] dump `rip`, `rsp`, `rflags`, and `cr3`; the current thread's CPU, id, and name; and the last 24 log records (F070)
- [x] under the `panic_exit` feature, `panic::finish` exits QEMU through `isa-debug-exit` with status 35 after the dump (F071, F141)

---

## Phase 6: Device Model and Buses

**Goal.** Find hardware, talk to it, and take interrupts from it without every driver reinventing
enumeration and DMA.

**Unlocks.** All of storage, network, and graphics.

**Exit gate**
- [x] `pci: <n> devices` marker and `lspci` output matching QEMU's configuration
- [x] MSI-X interrupts delivered to a chosen CPU, verified in-guest
- [x] a virtio device negotiated through modern PCI capabilities with a working virtqueue
- [x] DMA buffers allocated, mapped, and verified for correct device-visible addresses
- [x] a driver bound to a device automatically by id match, not by hardcoded probing order
- [x] workqueue and threaded IRQ handlers exercised in-guest

### 6.1 Device model
- [x] `Device` with a bus address, ids, resources, and an interrupt binding
- [x] `Driver` trait: `probe`, `remove`, plus an id match table
- [x] a registry matching drivers to devices, with driver init ordered by dependency; the order is per driver, and §20.2, §20.3, §20.9, and §25.4 order devices by DESIGN §12.2
- [ ] resource tracking so two drivers cannot claim the same BAR. Reopened by the kernel review (F115); lands in §10.12.
- [x] a device tree dump for the shell

### 6.2 PCI and PCIe
- [x] legacy configuration access through ports `0xCF8` and `0xCFC`
- [x] ECAM through the base of the first MCFG allocation, with addresses that match the PCI Firmware spec when that allocation starts at bus 0; only ECAM reaches offsets `0x100` to `0xFFF`, while mechanism #1 (`0xCF8`/`0xCFC`) reaches every bus (F045, F114)
- [x] recursive bus enumeration across bridges, covered by host tests; the kernel reads a bus other than 0 only through ECAM (F114)
- [x] BAR decoding: memory versus I/O, 32 versus 64 bit, size probing of memory BARs and of I/O BARs whose bits 31:16 read back set, `ioremap` of memory BARs (F113)
- [x] capability list walk: MSI, MSI-X, PCIe, vendor specific, power management
- [x] enable bus master and memory space in the command register
- [x] `lspci` with vendor and device names for the handful worth naming

### 6.3 MSI and MSI-X
- [x] MSI configuration: message address, message data, enable
- [x] MSI-X table in a BAR, per-vector address and data, per-vector mask
- [x] allocate vectors from the dynamic pool, bound to a chosen CPU
- [x] fall back to legacy INTx through the I/O APIC when a device has neither
- [x] `free_vector` masks the I/O APIC GSI before dropping an INTx route
- [x] in-guest test: trigger a device interrupt, assert it arrived on the intended CPU
- [x] interrupt affinity API, so phase 19 can rebalance without redesign

### 6.4 DMA
- [x] `DmaBuffer`: physically contiguous, known device address, explicit coherency
- [x] allocation from the buddy allocator with an alignment and boundary constraint
- [x] `sync_for_device` and `sync_for_cpu` as explicit calls even when they are no-ops on x86, because aarch64 will need them
- [x] scatter-gather list construction for devices that support it
- [x] barriers around descriptor publication, using the right fences rather than `compiler_fence` everywhere
- [x] IOMMU support deferred to §18.1, with the address translation kept behind an interface so it can be inserted

### 6.5 virtio
- [x] modern virtio over PCI: common configuration, notify, ISR, and device-specific capability regions
- [x] feature negotiation with the `VIRTIO_F_VERSION_1` handshake and a clear failure when features are missing
- [x] split virtqueue: descriptor table, available ring, used ring
- [ ] queue setup, kick, and completion handling with the barriers virtio 1.2 requires. Reopened by the kernel review (F016); lands in §10.3.
- [x] indirect descriptors, and `VIRTIO_F_EVENT_IDX` for interrupt suppression
- [x] the ring index arithmetic in the library half, host-tested against a simulated device

### 6.6 Deferred work
- [x] a workqueue: kernel threads consuming queued work items
- [x] threaded IRQ handlers, where the top half acknowledges and wakes a thread that may allocate and block
- [x] softirq-equivalent for latency-sensitive deferred work such as network receive
- [x] a documented rule for which handlers may block, because getting this wrong is a class of bug rather than an instance

---

## Phase 7: Block Storage

**Goal.** Bytes that survive a reboot, behind an interface a filesystem can use.

**Unlocks.** Filesystems, swap, a real userspace on disk.

**Exit gate**
- [x] `block: <name> <n> sectors` for each detected device
- [x] write a pattern, reboot the VM, read it back intact
- [ ] GPT and MBR partition tables parsed and exposed as separate block devices. Reopened by the kernel review (F081, F117); lands in §10.4 and §10.12.
- [ ] concurrent reads and writes from multiple threads with no corruption, verified in-guest. Reopened by the kernel review (F002); lands in §10.10.
- [x] the cache demonstrably reduces device requests, measured by a counter not by assertion
- [x] host tests: partition table parsing including deliberately corrupt tables, request merging, cache eviction

### 7.1 Block layer
- [x] `BlockDevice` trait: logical block size, capacity, read, write, flush, discard
- [x] a request structure with a completion, supporting both blocking and async submission
- [x] per-device request queue with a C-LOOK elevator and adjacent-request merging, capped at 8 segments and 8 waiters per merged request (F043, F119)
- [x] barrier and flush semantics written down in `src/block.rs` and DESIGN §10.2, which a journaling filesystem depends on; the host tests `barrier_holds_later_requests` and `flush_is_fence_then_device_op` check `block::Queue`'s dispatch order around one fence (F043)
- [x] a ramdisk implementation first, so the layer is testable before any real driver
- [x] error propagation with retry, and a device marked failed rather than retried forever

### 7.2 virtio-blk
- [x] probe and configuration read: capacity, block size, topology
- [x] request submission through the virtqueue with proper descriptor chaining
- [x] completion through the interrupt path into request completions
- [x] multi-queue with one queue per CPU, on a device that takes the queue from the notify address, as QEMU's virtio-blk does (F047)
- [x] flush and discard support
- [x] in-guest: sector roundtrip, unaligned multi-sector, deep queue with concurrent submitters

### 7.3 Partitions
- [ ] MBR parsing including extended and logical partitions. Reopened by the kernel review (F117); lands in §10.12.
- [x] GPT parsing with header and entry CRC validation and backup header fallback
- [ ] partitions exposed as offset-limited block devices. Reopened by the kernel review (F081); lands in §10.4.
- [x] type GUID recognition for the ones that matter
- [x] host tests over real table images, including truncated and CRC-broken cases

### 7.4 Cache
- [x] a page-granular cache over `ram0` and `vda`, keyed by device id and offset, which reaches the two drivers through `cache_init`'s `raw_read`, `raw_write`, and `raw_flush` rather than `BlockDevice` (F081)
- [ ] read-through, write-back with an explicit flush, and dirty tracking. Reopened by the kernel review (F015); lands in §12.5.
- [x] LRU eviction with a clock or second-chance approximation
- [x] readahead on detected sequential access
- [x] a writeback thread with a bounded dirty ratio
- [ ] built so Phase 12 can unify it with the page cache rather than maintaining two caches. Reopened by the kernel review (F015); lands in §12.5.
- [x] hit and miss counters exposed in the shell

---

## Phase 8: Filesystems

**Goal.** Paths, files, directories, mounts. Then a filesystem of our own that does not have FAT's
limitations.

**Unlocks.** Loading binaries. A userspace that persists. Configuration.

**Exit gate**
- [ ] mount a FAT32 image and `ls`, `cat`, `mkdir`, `rm`, `cp` behave correctly. Reopened by the kernel review (F058, F126); lands in §10.4, and its proof mounts from a thread started with `spawn`'s 16 KiB stack.
- [x] host tests write an image through the `vibeos-core` FAT code and require host `fsck.fat -n` to report it clean, and skip that check when `fsck.fat` is not installed (F143)
- [x] `/dev`, `/proc`, and `/tmp` populated by their respective filesystems in the `Vfs` mount table, which in-guest tests and the kernel shell's `ls` reach and no syscall does (F056, F086)
- [ ] vibefs survives injected power loss during a write, verified by a crash-consistency test. Reopened by the kernel review (F014, F080); lands in §10.2 and §10.11.
- [x] path resolution in `Vfs` handles `.`, `..`, symlinks, mount point crossing, and a symlink loop, which fails with `FsError::Loop` once a walk follows more than `MAX_SYMLINK` (8) links (F056)
- [x] host tests over synthetic filesystem images, including deliberately corrupted ones
- [ ] tag `phase-8` and release `v0.8.0`

### 8.1 VFS
- [x] `Inode` with a type, size, mode, times, and link count
- [x] `Dentry` with a name-to-inode cache and negative caching
- [x] `Superblock` per mount, with a mount table and mount point crossing
- [x] `FileSystem` trait for mount and root lookup; `InodeOps` for lookup, create, unlink, read, write, truncate, readdir, stat
- [x] path resolution with a bounded symlink depth
- [ ] `File` with an offset and flags, and a descriptor table that Phase 9 hands to processes. Reopened by the kernel review (F057, F086); lands in §10.4.
- [x] reference counting and a defined lifetime for an unlinked-but-open file opened through `Vfs`, which serves ramfs and kernfs files (F013, F067)
- [ ] inode and dentry caches with eviction, since an unbounded cache is a slow memory leak. Reopened by the kernel review (F065); lands in §10.4.

### 8.2 FAT32 read
- [ ] BPB parsing and validation. Reopened by the kernel review (F064); lands in §10.2.
- [x] FAT chain walking with a cluster cache
- [x] directory entry parsing, including ASCII long file names and their checksum validation (F054)
- [x] file read across cluster boundaries
- [ ] `readdir`, `stat`, timestamp conversion. Reopened by the kernel review (F123); lands in §10.4.
- [x] the on-disk structure parsing in the library half, host-tested against a generated image

### 8.3 FAT32 write
- [x] cluster allocation with a free cluster hint, FAT chain extension
- [x] file create, write, truncate, and delete through one open of a file at a time (F013)
- [x] directory create and delete, including long file name entry generation for ASCII names (F054)
- [x] both FAT copies and `FSInfo` kept in sync
- [ ] flush ordering that does not leave a directory entry pointing at unallocated clusters. Reopened by the kernel review (F013, F059); lands in §10.4 and §10.11.
- [x] verified by mounting the result on the host and running `fsck.fat`

### 8.4 Pseudo filesystems
- [x] `devfs` mounted in `Vfs`, where in-guest tests reach it: `null`, `zero`, `random`, and `urandom` nodes; `console` and `tty` nodes that read 0 bytes and reach no console until §13.7; and block-device nodes whose read and write return `NotSupp` (F081, F086)
- [ ] `tmpfs` on a private four-page instance of the Phase 7 cache type over a fixed 64 KiB store, so writes evict through the cache instead of growing a buffer; the reclaimable page-cache version is §12.5. Reopened by the kernel review (F066); lands in §10.4.
- [x] `procfs`: per-process directories, `cmdline`, `status`, `maps`, `fd`  (a pid-1 stub until §13.9 backs it with the process table)
- [x] `sysfs`-equivalent for the device tree and driver bindings
- [x] a `kernfs`-style shared implementation so the four do not duplicate directory logic

### 8.5 vibefs
- [x] the case for it: FAT32 has no permissions, no symlinks, no journaling, and no checksums, and every one of those becomes a wall
- [x] on-disk format documented in `docs/` before any code, with an explicit version field
- [ ] extent-based allocation rather than block lists. Reopened by the kernel review (F051); lands in §10.11.
- [ ] B-tree directories, so a large directory is not a linear scan. Reopened by the kernel review (F067); lands in §14.8.
- [ ] checksums on metadata and optionally on data, with corruption reported rather than propagated. Reopened by the kernel review (F063); lands in §10.11.
- [ ] copy-on-write metadata updates with atomic superblock switching, chosen over a write-ahead journal in VIBEFS.md §2. Reopened by the kernel review (F014, F049); lands in §10.11.
- [x] snapshots, which fall out nearly free from copy-on-write
- [x] inline data for files of at most 128 bytes, moved to an extent when a write ends past byte 128 (F062)
- [x] a host-side `mkfs` and `fsck` sharing the same format code as the kernel, so they cannot disagree
- [ ] crash consistency testing by killing QEMU at randomized points during a write workload, then checking with `fsck`. Reopened by the kernel review (F014, F080); lands in §10.2 and §10.11.

### 8.6 File API and shell
- [x] kernel-side open, read, write, seek, close, stat, readdir, mkdir, unlink, rename, symlink, link, truncate
- [ ] shell commands: `ls -l`, `cat`, `cp`, `mv`, `rm -r`, `mkdir -p`, `touch`, `stat`, `df`, `sync`. Reopened by the kernel review (F059, F126); lands in §10.4 and §10.11.
- [ ] shell commands `mount` and `umount`. Reopened by the kernel review (F058, F060, F126); lands in §10.4 and §13.9.
- [x] tab completion over the current directory, which is disproportionately useful when debugging by hand
- [x] an initial ramdisk image built by the Makefile and mounted at boot, so there is a root filesystem before block drivers are trustworthy

---

## Phase 9: User Mode and Processes

**Goal.** Untrusted code in ring 3, isolated by hardware, talking to the kernel only through syscalls.

**Unlocks.** Everything that makes this an operating system rather than a large program.

**Exit gate**
- [x] a static ELF binary loaded from the filesystem runs in ring 3 and exits with a status the kernel reports
- [x] `write` to fd 1 from userspace reaches the console
- [x] `fork` and `exec` produce a child that runs a different binary; the parent's `wait` returns its status
- [x] a user process faulting on a bad pointer is killed with a diagnostic and does not take the kernel with it
- [x] the interactive shell runs as a user process, not in the kernel
- [ ] every pointer argument is validated: an in-guest test gives each pointer argument of `read`, `write`, `open`, `execve` (path and `argv`), `wait4` (status), and `psinfo` a kernel-half pointer, an unmapped pointer, a range that crosses `USER_END`, NULL (except `execve`'s `argv` and `wait4`'s status, which accept NULL as on Linux), and, for a pointer with a length, a huge length; every call returns `-EFAULT` and the kernel stays up. Today only `write` is tested (`syscall_ptr_validate`, `/bin/tests`), with NULL, a kernel-half pointer, an unmapped pointer, an overflowing range, and a huge length; no syscall test passes a range that crosses `USER_END`. Reopened by the kernel review (F077); lands in §10.5.
- [x] `ps` typed into the user `/bin/sh` prints one line per process-table entry: pid, parent pid, state (`run`, `stop`, or `zombie`), and name, read through syscall 500 (`SYS_PSINFO`)
- [ ] tag `phase-9` and release `v0.9.0`

### 9.1 Ring 3 plumbing
- [x] user code and data selectors already in the GDT, verified against the `sysret` layout
- [x] `IA32_STAR`, `IA32_LSTAR`, `IA32_FMASK` configured; `EFER.SCE` enabled
- [x] `syscall` entry: `swapgs`, switch to the kernel stack from `PerCpu`, save the user context, dispatch
- [x] exit path restoring the context and `sysretq`, with an `iretq` path when the saved RFLAGS has RF or VM set (F007)
- [ ] a non-canonical saved RIP gets `SIGSEGV` and never reaches `iretq`; today the exit path sends it to `iretq`, whose `#GP` on KVM and hardware halts the kernel on the user GS base. Reopened by the kernel review (F007); lands in §10.6.
- [x] `RSP0` in the TSS updated on every context switch so an interrupt in ring 3 lands on the right kernel stack
- [x] x87 and SSE state saved and restored eagerly with `fxsave64` and `fxrstor64` at syscall entry and exit and on every context switch (`switch_fpu`), with no `xsave` and no lazy switching; the kernel itself stays soft-float on the built-in `x86_64-unknown-none` target (B2)
- [x] SMEP/SMAP/UMIP where CPUID allows, `CR0.WP` asserted on every CPU, `stac`/`clac` helpers (S1)

### 9.2 Address spaces
- [x] `AddressSpace` owning a PML4, with the kernel half shared by mapping the same upper entries
- [x] user mappings tracked as regions with permissions and a backing source, not just raw PTEs
- [x] CR3 switch on context switch, skipped when the next thread shares the address space
- [x] teardown freeing every user frame and page table page, verified against the frame count
- [x] user pointer helpers: `AddressSpace::check_user_range` rejects a non-canonical, kernel-half, overflowing, or null-guard range and requires a present `USER` leaf on every page, and `read_bytes`, `write_bytes`, and `zero_bytes` copy through the physmap only after it passes, so a syscall given a bad pointer returns `EFAULT` without a kernel page fault (F023)
- [ ] user copies dereference the user address inside `stac`/`clac` and turn a fault into `EFAULT` through an exception-table fixup, so a write into a user page mapped without `WRITABLE` fails; today `write_bytes` ignores `WRITABLE`, and `read`, `wait4`, and `psinfo` can write into the caller's own text. Reopened by the kernel review (F023); lands in §10.6.
- [x] a guard region at address 0 so a null dereference faults rather than reading something

### 9.3 Syscall ABI
- [x] the ABI documented in `docs/`: register assignment, return convention, error encoding
- [ ] a dispatch table indexed by number, with an arity and a validation policy per entry, that syscall dispatch reads, checking each pointer argument before the handler runs; today `proc_init::dispatch_frame` is a `match nr`, the `SyscallInfo` fields `arity`, `ptr_mask`, and `len_arg` and `validate_args` in `src/syscall.rs` are read only by host tests, and `open`, `execve`, and `wait4` have `ptr_mask: 0`. Reopened by the kernel review (F150); lands in §10.5.
- [x] first set wired for proof: `write`, `exit`, `getpid`, `sched_yield`
- [x] remainder: `read`, `open`, `close`, `lseek`, `fork`, `execve`, `wait4`, `getppid`, `dup`, `dup2`, `kill`, `fcntl`
- [ ] `brk`, anonymous `mmap`/`munmap`, `getdents64`, `fstat`, and `nanosleep`: land in §10.5
- [ ] `openat`, `dup3`, and fork-shaped `clone`: land in §11.6
- [ ] `stat` and the rest of the POSIX floor: land in §13.9
- [x] errno numbers equal Linux's for every name `src/syscall.rs` defines (F083)
- [ ] each error condition returns the errno Linux returns for it; today a full filesystem, a full global open-file table, and FAT's 4 GiB file limit return `EMFILE` instead of `ENOSPC`, `ENFILE`, and `EFBIG`, on-disk corruption returns `EINVAL` instead of `EIO`, and `lseek` on the console returns `EINVAL` instead of `ESPIPE`. Reopened by the kernel review (F083); lands in §10.4.
- [x] every pointer argument range-checked against the caller's address space before use: canonical, in the user half, above the null guard, no overflow, and a present `USER` leaf on every page; the `WRITABLE` bit of a destination is not checked, which §9.2's open accessor box fixes (F023)
- [ ] syscall tracing behind a flag that a §10.2 command-line option sets, printing one `user: syscall` line per call with its name, number, and return value; today `syscall_init::set_trace` has no caller, so the trace never prints. Reopened by the kernel review (F150); lands in §10.7.
- [ ] a syscall count per process, summed over its threads and reported through syscall 500 until §13.9 moves it to procfs; today the per-TCB `syscall_count` and the global `SYSCALLS` are incremented on every syscall and never read. Reopened by the kernel review (F150); lands in §10.7.

### 9.4 ELF loader
- [x] ELF64 header validation: class, endianness, machine, type
- [x] `PT_LOAD` segments mapped with permissions from the flags, honoring `p_filesz` versus `p_memsz` zero fill
- [x] `PT_GNU_STACK` respected for stack executability
- [x] an image with `PT_INTERP` refused with `ElfError::HasInterp` rather than run without its interpreter (dynamic loading is §14.2)
- [x] initial stack set up with `argv`, an empty `envp`, and the auxiliary vector
- [ ] `execve` copies the caller's `envp` onto the new image's initial stack, as it copies `argv` (lands in §10.5; today `sys_execve` discards `envp` and `fill_stack` passes an empty one)
- [x] `PT_TLS` parsed by `elf::parse`, and `setup_tls` places its image below the stack in the x86_64 variant II layout with a self-pointer at the thread pointer; `FS_BASE` is loaded at the first entry to ring 3 and at `execve` (F022)
- [ ] `FS_BASE` saved and restored per thread on every context switch, and `fork` gives the child the parent's saved value, shown by an in-guest test that runs a `PT_TLS` binary across another process's exit; today `on_switch` does not switch it, so a TLS process resumes with whatever base its CPU last loaded, 0 after any process on that CPU exits or is killed. Reopened by the kernel review (F022); lands in §13.1.
- [x] header parsing in `vibeos-core` (`src/elf.rs`), host-tested against images the test helper `build_elf` synthesizes and against truncated ones
- [ ] host tests that parse checked-in binaries linked by `ld.lld` and by GNU `ld` (lands in §10.5; today every host-test image comes from `build_elf`)
- [x] refuse a malformed binary with an error rather than mapping garbage

### 9.5 Process abstraction
- [x] `Proc`: pid, parent pid, address space, descriptor table, a `cwd` that `fork` copies, credentials, and wait status (F086)
- [ ] a relative path in a syscall resolves against the caller's `Proc.cwd`; today `file_init::join_cwd` resolves it against one kernel-global CWD, which the kernel shell's `cd` sets. Reopened by the kernel review (F086); lands in §10.4.
- [x] each process owns one thread (`Proc.tid`), and the address space, descriptor table, and credentials belong to the process (F077)
- [ ] a process is one or more threads sharing an address space. Reopened by the kernel review (F077); lands in §13.1.
- [x] the descriptor table with per-fd flags, `dup`, `dup2`, and close-on-exec
- [x] a process tree: when a spawned process exits, `finish_exit` reparents its children to pid 1 and wakes init's wait queue (F068, F127)
- [ ] a bound (`run_path`) process that exits or is killed reparents its children to init; today `finish_exit`'s bound branch leaves through `longjmp_user` or `return_status_or_die` before `reparent_children`, and the next process given that pid adopts the children. Reopened by the kernel review (F127); lands in §10.6, whose one ring-3 entry model box deletes the bound model, `finish_exit`'s bound branch included.
- [ ] an orphan goes to pid 1 only while init is live or stopped and is otherwise freed when it exits; today `reparent_children` sets `ppid = 1` even when init is dead, or absent as in the `kernel_tests` and `kernel_shell` builds. Reopened by the kernel review (F068); lands in §10.5.
- [ ] init's own exit panics the kernel with a registered marker; lands in §10.5 (F068)
- [x] zombie state until reaped, and a defined resource release point
- [x] uid and gid present from the start even if nothing enforces them yet, because retrofitting credentials is painful

### 9.6 fork, exec, wait
- [x] `fork`: clone the address space, duplicate descriptors, copy the general registers from the syscall frame, and return 0 in the child (F069)
- [ ] `fork` copies the parent's x87 and SSE state into the child; today the child starts from the boot FPU template. Reopened by the kernel review (F069); lands in §10.6.
- [x] `fork` makes a full copy (`clone_full`) until §12.3 adds copy-on-write behind the same interface
- [x] `execve`: build the new address space first, and only replace the old one after the load succeeds, so a failed exec leaves the caller intact
- [ ] `execve` starts the new image with the psABI initial FP state (FCW `0x037F`, MXCSR `0x1F80`, x87 and XMM registers zeroed); today it keeps the old image's x87 and XMM registers, MXCSR, and FCW; lands in §10.6 (F069)
- [x] `exit`: release resources, become a zombie, signal the parent
- [x] `wait4`: block for a child, return its status, reap it, with `WNOHANG`
- [x] orphan reaping by `/sbin/init` in the production image; the `kernel_tests` and `kernel_shell` builds start no init (F068)
- [ ] `/bin/tests` fork limit: a fork loop gets `-EAGAIN` once the process table is full and fails on any other result; today the bomb in `user/tests.asm` also passes after 32 forks that all succeed, and on any error after the first. Reopened by the kernel review (F077); lands in §10.5.
- [ ] `/bin/tests` exec chain: a child `execve`s a second program, which `execve`s a third, and the parent's `wait4` returns the third program's exit status. Reopened by the kernel review (F077); lands in §10.5.
- [ ] `/bin/tests` wait ordering: `wait4` on a child that has not exited blocks until it exits, and `wait4` on a zombie child returns its status at once. Reopened by the kernel review (F077); lands in §10.5.
- [ ] `/bin/tests` orphan reparenting: a grandchild whose parent has exited sees `getppid() == 1`, and init reaps it. Reopened by the kernel review (F077); lands in §10.5.

### 9.7 Early signals
- [x] `SIGKILL` and `SIGSTOP` handled in the kernel with no user handler, applied when the target next enters a syscall or wakes in `wait4`
- [ ] a pending signal whose default action terminates or stops the process takes effect on every return to ring 3, interrupt returns included, so a process that makes no syscall can be killed or stopped; today `apply_pending` runs only at syscall entry and after the `wait4` sleep; lands in §10.6 (F033)
- [ ] `/bin/tests` signals: `SIGKILL` ends a child that loops on `sched_yield`, and `SIGSTOP` then `SIGCONT` stops a child and resumes it; lands in §10.5 (F077)
- [x] `SIGSEGV` from `#GP` and `#PF`, `SIGBUS` from `#NP` and `#SS`, `SIGFPE` from `#DE`, and `SIGILL` from `#UD`, mapped by `sig_for_vec` (F026)
- [ ] `SIGFPE` from a user x87 `#MF` and a SIMD `#XM`, with `CR0.NE` and `CR4.OSXMMEXCPT` set on every CPU; today neither bit is set, so an unmasked x87 exception raises the masked IRQ13 and is dropped, and on KVM and hardware an unmasked SIMD exception raises `#UD` and gets `SIGILL`. Reopened by the kernel review (F026); lands in §10.6.
- [x] `SIGCHLD` on child exit
- [x] default actions: terminate, ignore, stop
- [ ] a signal whose default action terminates or stops is dropped when sent to pid 1, as Linux drops a signal init has no handler for; today any ring-3 process can kill or stop init; lands in §10.5 (F068)
- [ ] user-installed handlers, masking, and queueing: land in §13.8

### 9.8 First userspace
- [x] a minimal freestanding user program with hand-written syscall stubs and no libc, to prove the path
- [x] a userspace test runner, `/bin/tests`, that calls `write`, `getpid`, `dup`, `close`, `fork`, `execve`, `wait4`, `exit`, and `sched_yield`, with `EBADF` and `EFAULT` cases for `write` only (F077)
- [ ] `/bin/tests` calls every §9.3 syscall and asserts each errno it can return; today it never calls `read`, `open`, `lseek`, `dup2`, `fcntl`, `kill`, `getppid`, or `psinfo`. Reopened by the kernel review (F077); lands in §10.5.
- [x] the shell moved out of the kernel and into a user process, keeping the kernel one only under a debug feature
- [x] the kernel's job after init becomes starting `/sbin/init` and nothing else

---

## Phase 10: Consolidation

**Goal.** Pay down what Phase 0 laid down in a hurry, before Phase 11 adds a second architecture and
Phase 14 a userspace with a C library. The work has three sources. The 2026-09-22
[architecture review](reviews/ARCHITECTURE_REVIEW.md) has its per-item plans under
[reviews/issues/](reviews/issues/README.md), and the letter codes below name them. The
[roadmap review](reviews/ROADMAP_REVIEW.md) is the source of the entry paths and user memory, the user
runtime, and the CI budget. The 2026-09-23 [kernel review](reviews/KERNEL_REVIEW.md) adds a line for
each bug it found in the code, in Phase 10 or in the later phase that first needs the fix. A finding id (Fnnn) names the plan for each line that cites it, as the letter codes do for the
issue files; where a box and its finding's Fix differ, the box decides. §10.7 to
§10.9 (forensics, models and proofs, and the engineering system), the §10.2 command line, and the §10.2
deterministic build have no issue files; their boxes are the plan. All of it makes the next
three phases checkable.

**Unlocks.** A `vibeos-core` crate that a second architecture can share. A user runtime that can
express the Phase 12 and Phase 13 gates. Tables that do not cap a shell pipeline at sixteen processes.
Kernel entry and user-memory paths that copy-on-write and demand paging can build on without a
security hole. Block completions and thread-stack reclaim that Phase 13 can run on every CPU. A boot
that writes no disk it did not format. Hangs that explain themselves from the host. The seqlock, the
wake inbox, and the log ring model-checked before Phase 11 runs them on a weakly ordered CPU.

**Order.** Phase 10's parts are not all independent, so it lands in three waves:

1. First, before any other Phase 10 PR merges: every box in Phases 0 to 10 that cites a CRITICAL
   or HIGH finding without a LATENT tag (the set `scripts/check_review_refs.py --closed` checks:
   data destruction, the syscall exit and entry-path halts, the completion and stack-reclaim
   use-after-frees, the exhaustion panics, the vibefs and FAT leaks, and the cell soundness bounds),
   the boxes those land after (below), and §10.2's box that writes every harness retry to the job
   summary, so no fix is judged by a run that retried. The wave ends when
   `check_review_refs.py --closed` passes.
2. Then the refactors that move files: A1's directory move, Q5's splits, A4's cycle breaks, and Q2
   and T1's per-subsystem test modules, merged back to back as pure moves while no other Phase 10
   PR is open, so the path churn conflicts once instead of with every open branch.
3. Everything else, in parallel and in any order its lands-after clauses allow.

A LATENT finding's fix lands in the phase its milestone names. When a Phase 10 box reaches that
milestone, the fix moves into Phase 10 and the box lands after it, or with it. §10.6's IF=1 syscall
bodies reach F033, F055, and F106, so the syscall-body box lands after §10.3's teardown box (F106),
§10.4's open-file table box (F055), and §10.6's stop-wait box (F033), which are wave 1 with it.
§10.4's path routing reaches F065 and F066, so it lands after the dentry-cache and tmpfs boxes.
§10.6's counted address space (F019) lands ahead of its milestone, Phase 12's reverse map, because
Phase 11 ports the address-space code and Phase 12's fault path, `fork`, and reverse map are written
against it.

A wave-1 fix lands in today's file layout and wave 2 moves it with the rest. Why this order: the
wave-1 bugs are live in `main` today (README), and a file move merged in the middle of them would
force every fix branch to rebase across it.

**Exit gate**
- [x] `make check` (fmt, clippy with warnings denied, host units, harness units) gates CI ahead of the QEMU ladder
- [ ] `make check` passes on macOS and Linux, and a scheduled macOS CI job proves it
- [ ] `make test` passes on the scheduled macOS CI job, with Homebrew's QEMU and its code-only `edk2-x86_64-code.fd` as the UEFI firmware (F079)
- [x] the nightly is pinned by date; Limine and every GitHub action are pinned by hash
- [ ] `vibeos-core` contains no inline assembly and no `cfg(target_arch)`
- [ ] `vibeos-core` builds and passes its host tests against the §10.3 stub `arch`, in `make check`
- [ ] syscall copies to and from the calling process go only through the §10.6 accessors, and only the named fill API writes to an address space that is not running. In-guest tests show three things. SMAP faults a stray kernel data access to a user page, including one from an exception handler entered with the accessor window open. SMEP faults a kernel jump into a user page. A kernel-half pointer, and `read()` into a read-only user page, both return `EFAULT`
- [ ] no user context reaches `iretq` or `sysretq` with a non-canonical RIP, and the kernel survives an ELF whose last page holds a `syscall` (§10.6)
- [ ] ring 3 cannot halt the kernel through an exception: the in-guest tests `user_single_step` (`RFLAGS.TF` set with `popf`), `user_int1`, and `user_exceptions` (`int3`, `#DE`, `#UD`, `#GP`, `#PF`, x87 `#MF` raised in ring 3, and SIMD `#XM` in the KVM leg only) each find the program killed by its signal and the kernel running, in `make test-kernel` and `make test-kernel-smp4` under TCG and in the §10.1 KVM leg's `make test-kernel` (§10.5, §10.6)
- [ ] ring 3 cannot halt the kernel through an interrupt or a return to user mode: the in-guest tests `user_device_irq` (a pool vector and the keyboard vector that CPU 1 sends while CPU 0 runs ring 3 with IF=1), `user_ipi` (a reschedule IPI taken at CPL 3), `console_read_exit` (the return from a `read()` that halted in `wait_key`), and `user_entry_irq` (1,000 forked children entering ring 3 under reschedule IPIs from another CPU) pass in `make test-kernel` and `make test-kernel-smp4` under TCG and in the §10.1 KVM leg's `make test-kernel` (§10.6)
- [ ] ring 3 cannot halt the kernel by exhausting a resource: the in-guest tests `exit_burst` (16 processes exit back to back, each handing off to a sibling resumed from timer preemption), `fork_oom` (a `fork` whose kernel-stack allocation fails returns `ENOMEM` and leaves the free-frame count at its baseline), and `exec_huge_memsz` (an exec with a 64 GiB `p_memsz`, and one under the §10.6 cap but larger than free memory, each return `ENOMEM` and leave the free-frame count at its baseline) pass in `make test-kernel` and `make test-kernel-smp4` under TCG and in the §10.1 KVM leg's `make test-kernel` (§10.6, §10.10)
- [ ] the §10.10 completion, stack-reclaim, and TLB-shootdown-ack tests, selected with `VIBEOS_KTEST` and run with `VIBEOS_KTEST_REPEAT=20`, pass in `make test-kernel-smp4` under TCG and under KVM on the §10.1 KVM leg, which runs that target for this line, and those whose box does not name `make test-kernel-smp4` also pass in `make test-kernel` (§10.10)
- [ ] a boot of the production ISO leaves `vda` byte-identical when it holds a 1 MiB whole-disk vibefs image, and when it holds a 1 MiB image whose only non-zero bytes are `0x55AA` at offset 510 (the empty partition table `part` also finds on a whole-disk FAT32 volume): an e2e case in `make test-e2e` boots once with each image and compares its SHA-256 before and after (§10.11)
- [ ] a file offset cannot halt the kernel: the §10.11 in-guest tests on `/vibe` get `EFBIG` from a `write` after `lseek` to `2^44 - 4096`, `EINVAL` from `lseek` to `2^44`, and 5 GiB + 1 from `lseek(fd, 0, SEEK_END)` after a one-byte `write` at offset 5 GiB, in `make test-kernel` (§10.11)
- [ ] `make test-vibefs-crash` passes, and fails against a `vibefs_crash` build with a planted leak of one block per commit, which this line's §10.9 gate-map entry builds and runs (§10.2, §10.11)
- [ ] the growable tables are heap-sized at init from one `limits` module, and this gate states the limits Phase 12 is tested against: 256 processes, 256 descriptors per process, 1024 threads, 256 regions per address space
- [ ] FAT and vibefs implement `InodeOps`; the `Back` enum in `file_init` is gone; every file operation goes through `Vfs`
- [ ] one errno-shaped kernel error type; the errno table in `docs/SYSCALL.md` §2 is generated from it
- [ ] a Rust user runtime replaces the four assembly programs
- [ ] `utest_*` results are asserted by the harness the way `ktest_*` are, in every e2e variant that reaches `shell ready`, from the `/bin/tests` that `/sbin/init`, pid 1, forks (F073)
- [ ] the user `/bin/sh` runs programs from `PATH` and reports their exit status
- [ ] `make test` is green with no retry left in the harness, and `make test-kernel` and `make test-kernel-smp4`, each with `VIBEOS_KTEST_REPEAT=20`, run the in-guest suite 20 times in one boot under TCG with no failure on the weekly job (§10.2)
- [x] SMEP, SMAP, and UMIP set on every CPU whose CPUID reports them, and `CR0.WP` on every CPU
- [x] the kernel VFS's `/dev/random` fed by virtio-rng, then `RDRAND`, then a xorshift fallback that §10.12 deletes (F134); no syscall reaches `/dev/random` until §10.4's A3 routes file syscalls through `Vfs` (F086)
- [x] `BootInfo` captured once; nothing outside `boot` reads a Limine response
- [x] `AGENTS.md` and an MIT `LICENSE` in the tree; README status current
- [ ] every item in [reviews/issues/README.md](reviews/issues/README.md) is marked implemented or declined with a reason: the prose status paragraph there becomes a Status column, `scripts/check_issues.py` in `make check` fails on a Status cell other than `proposed`, `implemented (#<PR>)` with one or more PR numbers, or `declined: <reason>`, and its `--closed` mode, which this line's §10.9 gate-map entry runs, fails on any `proposed` row
- [ ] every finding in the [kernel review](reviews/KERNEL_REVIEW.md) is traced: `scripts/check_review_refs.py` in `make check` fails on a finding id that no line of this file cites and on a cited id the review does not define, and its `--closed` mode, which this line's §10.9 gate-map entry runs, fails while an open box in Phases 0 to 10 cites a CRITICAL or HIGH finding without a LATENT tag
- [ ] the `hang_test` ISO at `-smp 4` under TCG hangs, and from the guest core and the kernel ELF, without the serial log, the harness prints a symbolized backtrace for every CPU, every thread's state, and the last 64 log records, and exports every CPU's trace ring as Chrome trace-event JSON (§10.7)
- [ ] `make models` and Miri are green on the nightly job: every loom model passes and fails its weakened variant, Kani proves the buddy allocator and the §10.6 user-range check to their stated bounds, and Miri passes over the portable crate's host tests (§10.8)
- [ ] `make gate PHASE=10` passes with an entry for every Phase 10 gate line but the tag, `cargo deny check` passes with advisories included, and `scripts/ci_history.py` finds a record for every `ci` run on `main` since the history landed (§10.9)
- [ ] tag `phase-10` and release `v0.10.0`

### 10.1 Gates and pinning
- [x] `rustfmt.toml`, one `cargo fmt` commit, `cargo clippy -- -D warnings`, and `-D warnings` in `[build].rustflags`, which reaches the host builds of `vibeos-core` and hostlib (Q1, F147)
- [x] a fast `check` CI job before the QEMU ladder, with an llvm-cov floor on the portable crate that only ratchets up (T3)
- [x] dated nightly; action SHAs; Limine commit verified after clone; cargo cache keyed on `Cargo.lock` (C1)
- [x] `make check` as the local gate; `ruff` and `mypy --strict` over `tests/` and `scripts/` when they are installed (DX1, F147)
- [x] restriction lints on the portable crate: no `unwrap`, `expect`, or `panic!` outside tests (E1)
- [x] `vibeos-core` enables no unstable feature (DESIGN §1.1): `scripts/check_core_stable.py`, which `make check` runs, fails on a feature attribute in `src/lib.rs`, so Kani, loom, and Phase 38's Verus keep building the crate the kernel links; `tests/harness/test_core_stable.py` tests it
- [ ] `-D warnings` reaches every kernel build: `"-D", "warnings"` joins `[target.x86_64-unknown-none].rustflags` in `.cargo/config.toml`, since Cargo reads one rustflags source and that table shadows `[build].rustflags` (Q1, F147)
- [ ] CI runs `cargo clippy --bin vibeos -- -D warnings` on the default features that ship, beside its `--all-features`, `kernel_tests`, and `vibefs_crash` runs (Q1, F147)
- [ ] `ruff` and `mypy` pinned by version in the `check` job, and `make check` fails when either is missing and `CI` is set (DX1, F147)
- [ ] `scripts/check_changelog.py` fails on a changelog entry longer than 2 lines, as the standing gate says; today its `MAX_LINES` is 3 (F147)
- [ ] the `unsafe` standing gate enforced by lint: `clippy::undocumented_unsafe_blocks` denied through `[workspace.lints.clippy]`, so every member inherits it (including `user/` from §10.5), and `check-private-items = true` in `clippy.toml`, so `missing_safety_doc` reaches the kernel binary's private items; a `// SAFETY:` line on every existing `unsafe` block, 653 at `90ce475` (F041)
- [ ] four safety comments corrected against the code: `per_cpu_init.rs` says `irq_nest_enter` is a no-op while GS is 0, but `try_current` reads `gs:[0]` once `LIVE` is set; `arch/gdt.rs` says hardware updates `TSS.RSP0`, which software writes; `paging.rs` says `Mapper` is not `Sync`, but it is auto-`Sync`; `cell.rs` calls `IrqCell` CPU-local, but it also guards cross-CPU tables through its owner CAS (F041)
- [ ] a nightly x86_64 KVM leg on the hosted x86_64 runner, whose `/dev/kvm` GitHub's documented udev rule opens to the runner user, that runs `make test-kernel` now and, from Phase 12 on, every benchmark and every gate number measured under KVM, since TCG and KVM each hide bugs the other finds; the timing tests that made the harness default to TCG are fixed first, which HVF in Phase 11 also needs. GitHub assigns each job's host CPU at random (AMD EPYC or Intel Xeon, several models), so the leg writes the CPU model from `/proc/cpuinfo` to its job summary and its §10.9 CI-history record; a regression threshold compares a number only with history from the same model, and a gate's fixed threshold holds on every model the leg draws
- [ ] the KVM leg also runs `make test-e2e` and `make test-lapic-fallback` (F078)
- [ ] `run_ktest.py` checks the `lapic_timer` marker's mode as `run_e2e.py` does: `tsc-deadline` under `-cpu max` and `periodic` under `qemu64,-tsc-deadline` (F078)
- [ ] an in-guest test finds every CPU's `PerCpu.ticks` advancing, which on the KVM leg shows that `arm_tsc_deadline`, `rearm_deadline`, and the `TscDeadline` arm of `arm_ap` run on every CPU; no TCG tier runs them, because TCG never advertises TSC-deadline (F078)
- [ ] the CI budget, written into DESIGN §8.6 in the same commit as the workflow change. Every push runs `check` and, alongside it, one `build` job per architecture (x86_64 now, aarch64 from Phase 11). The build job builds every ISO variant and the host `mkfs`/`fsck` tools once and uploads them. A matrix of tier jobs per architecture (`needs: [check, build]`, `fail-fast: false`, TCG) downloads them and runs the same `make test-*` targets through a prebuilt-ISO switch, so the Makefile stays the one definition of each tier. Tiers are grouped to about 40 s of QEMU each (x86_64 at `88370e5`: the five e2e boots; in-guest at `-smp 2` and `-smp 4` plus the LAPIC fallback; the vibefs crash test), and each gets its own check name, so a red PR names the failing tier. Everything else runs on a schedule: the macOS job, the KVM leg, the fuzzers, stress, and any job with a performance threshold. Later lines name two scheduled workflows, both on the pinned toolchain: the nightly job, which carries the KVM leg, and the weekly job (`smp-stress` today); the non-blocking `nightly-canary`, the one job on an undated nightly, is neither, and a line that needs its own workflow or another cadence names it (§20.8's `hardware-models`, §24.2's rebuilds). A later line that says "in CI" for a functional test means a ladder tier; for a benchmark or a threshold it means the KVM leg. A red scheduled job blocks the next phase tag. The earlier no-matrix rule (runner queues) is lifted: the repository is public, so standard runners are free and unlimited, and the limits that matter are 20 concurrent jobs on the Free plan (at most 5 macOS; scheduled campaigns together hold at most 10, so pushes keep the other 10) and 6 hours per job: a scheduled run longer than 5.5 hours is split into shards that hand their state on as artifacts, and the line that needs one says so; the scheduled workflows are staggered so that together they leave room under the 20 for a push's jobs, and DESIGN §8.6 records each one's schedule and peak job count and its share of the 10 scheduled slots; workflows that can run at the same time hold shares summing to at most 10, a multi-day workflow (§24.2's rebuilds) holds only its share, and a job that finds its share full waits in request order, since GitHub caps no job count across workflows
- [ ] CI wall time cut inside each job: host packages from a cache or a prebuilt image rather than `apt-get` on every run (about 20 s of every job at `88370e5`), and independent QEMU runs in parallel inside a tier once the timing tests and the §10.2 retried failures are fixed, since concurrent QEMU under TCG makes both more likely. Push-to-green wall time is recorded in DESIGN §8.6 before and after; the aim is under two minutes for x86_64, and the box does not wait for it (3m40s at `88370e5` with no retry; a retried hang adds 60 to 90 s)
- [ ] `make debug`: QEMU `-s -S` plus a `gdb` script that loads the kernel ELF and the user ELFs, documented in DESIGN §8.4
- [ ] every Linux CI job runs on GitHub's free `ubuntu-26.04` image (`ubuntu-26.04-arm` for arm64 jobs), whose apt QEMU 10.2.1 meets every QEMU minimum this file names (9.0 for the Phase 11 gate's EL2 boot and §20.1's boot with more than 255 vCPUs, 10.2 for §18.1's amd-iommu `dma-remap` and Phase 25's GHES injection) except a line that names QEMU 11.1 or later, whose jobs build that release from its tarball, checked by SHA-256 and cached by version; when `CI` is set on a Linux runner, the harness's shared QEMU launcher (§10.2) compares `qemu-system-* --version` with the version its job pins (Ubuntu's 10.2.1, or the built release) before its first boot and fails on a mismatch, so an image update that moves QEMU fails loudly; `make check`, the macOS job, and the dev host's Homebrew QEMU are left alone
- [ ] no workflow expands a `${{ }}` expression inside a `run:` script: `release.yml`'s changelog step takes the tag from `env: TAG: ${{ inputs.tag }}`, the dispatch input below, and passes `"$TAG"`, and `scripts/check_workflows.py` in `make check` fails on `${{` inside any `run:` block (F144); the same script fails on a workflow without a top-level `permissions:` block, and on any grant beyond `contents: read` that the workflow does not name a need for in a comment beside it (`ci.yml` and `smp-stress.yml` declare `contents: read`; `release.yml` declares `contents: read` and grants `contents: write` to its `publish` job alone, below)
- [ ] `release.yml` publishes `vibeos.iso` alone, and publishes nothing unless the `ci` workflow concluded `success` at the release tag's commit; today it runs only `make test-e2e` and also publishes `vibeos-ktest.iso`, whose block tests write fixed LBAs on any attached virtio-blk disk (F145)
- [ ] `release.yml` runs only from `main`, so the workflow that publishes, and from §14.6 signs, is always `main`'s, never a copy a tag carries from a branch cut long ago: its one trigger is `workflow_dispatch`, with the release tag as input, until §14.6 adds a `schedule` that only re-signs metadata. A `build` job with `contents: read` checks through the API that the tag is annotated and points at a commit on `main` (from §39.3, or on a supported branch) and that `ci` concluded `success` there, checks that commit out with `persist-credentials: false`, restores no cache, runs `setup.sh`, the tag tree's `make release-artifacts OUT=<dir>` (today `vibeos.iso`), and `scripts/changelog_section.py` for the notes, and uploads them with a list of their SHA-256 sums. A `publish` job with `contents: write` downloads them, checks them with `sha256sum -c`, and publishes them with the pinned release action; it checks out nothing and runs no repository script. `setup.sh` fails when `git -C limine status --porcelain --untracked-files=no` lists a changed tracked file, so a restored cache cannot change a Limine binary that the HEAD check passes (untracked files are left alone: the macOS build leaves `limine.dSYM/`); the `build` job, which restores no cache, also builds Limine's host tool fresh. `scripts/check_workflows.py` fails on `actions/cache`, a trigger other than `workflow_dispatch` (and, from §14.6, that `schedule`), or a workflow-wide write grant in `release.yml`, and on a job there with a write grant, or any job that names the `release` environment (§14.6), that checks out the repository or runs a repository script; host tests cover each rule. `docs/RELEASING.md` starts here with the owner's release steps (push the `phase-<N>` and `v0.<m>.0` tags, then dispatch `release.yml` from `main` with the release tag), and §14.6 extends it

### 10.2 Build and harness
- [x] built-in `x86_64-unknown-none` target; the custom JSON, `-Zbuild-std`, and `-Zjson-target-spec` deleted (B2)
- [x] one parametrized ISO recipe; a variant is one line (B1)
- [x] one QEMU launcher and one `VIBEOS_*` reader shared by every driver (T2, C2)
- [ ] one `target/` for every feature build: each variant's ELF is copied to a named output under `build/`, and every ISO recipe reads only its own named ELF, so a test build still cannot be packaged as production; DESIGN §8.2 and the pitfall that says "separate target directory" rewritten to that guard; `target/` is the only kernel target directory cached in CI (P1)
- [x] one initrd generator; the trampoline assembled by `global_asm!` so the kernel build no longer needs `nasm` (B4); the assembly user programs still do until §10.5
- [ ] a deterministic kernel, initrd, and ISO build (moved from §17.5): a scheduled job builds every ISO variant twice from one commit, with a different checkout path, `CARGO_HOME`, and `RUSTUP_HOME` each time, and fails unless the two builds are byte-identical; §17.5's second-generation comparison and §22.1's release artifacts build on it (F151, F152)
- [ ] no build time or builder identity in the initrd or the ISO: `SOURCE_DATE_EPOCH` from the commit, honored by `mkinitrd` and the ISO writer; `mkiso.sh` pins staged file times and passes `-r` and `--set_all_file_dates` from `SOURCE_DATE_EPOCH`, so Rock Ridge records no builder uid or gid; every input list sorted (F152)
- [ ] no random identifier in the ISO: fixed volume and GPT GUIDs; after `limine bios-install`, `mkiso.sh` overwrites the MBR disk signature at `0x1B8`, which Limine seeds from `time(NULL)`, with a value derived from the image; release builds record the xorriso version, which lands in the volume descriptor (F152)
- [ ] no host path in any artifact: Cargo's `trim-paths = "all"` in the dev and release profiles passes rustc a `--remap-path-prefix` for the checkout, the sysroot, and `$CARGO_HOME`, so panic `Location` strings, DWARF, and the ThinLTO `.llvm.<hash>` names `gen_ksyms.py` copies into `KSYMS` carry none (F151)
- [ ] the in-guest registry split per subsystem; a test's name printed before it runs; a per-test deadline (T1)
- [ ] the in-guest registry runs in production's interrupt context. `ktest::run` runs it on a spawned kernel thread with IF=1 and `irq_nest` 0, on a guarded 64 KiB KVA stack (the size Limine guarantees the boot stack it uses today; `spawn` gives 16 KiB), instead of on the bootstrap thread under an `InterruptGuard`, so `spawn_here` workers, which copy `irq_nest`, also run with IF=1. A test that needs interrupts off takes its own guard. `with_timer`, which runs `sti` under the registry's guard, is deleted, as is the `yield_now` in `sys_fork` with its comment about ktest's IF-off registry. T1's per-test deadline needs IF=1. This box lands after §10.6's one ring-3 entry model box, since until then `run_user` must run on the bootstrap thread and enters ring 3 with IF=0 (F075)
- [ ] `kernel_tests` hooks isolated in per-subsystem `ktest.rs` files; no blanket `allow(dead_code)` in production modules (Q2)
- [ ] the `catch` module and every `catch::intercept` call in `arch/idt.rs` compile only with `kernel_tests`, and Q2's `nm` check fails on any `arch::catch` symbol in the production ELF; today `intercept` runs first in every exception handler of every build (F146)
- [ ] `catch::intercept` and `catch::on_panic` act only on a CPL 0 frame on the CPU that armed the catch, so a fault on another CPU during a catch window takes its normal path (F146)
- [ ] `block_vblk_deep` waits for every request it submitted before it returns a failure, so no completion reaches an `IoWaiter` in its dead frame (F146)
- [ ] a kernel command line: `cmdline:` in the `limine.conf` entry, captured in `BootInfo` through Limine's executable command line request, then an `opt/vibeos/cmdline` fw_cfg file appended when QEMU's fw_cfg is present (probed on x86_64 only when CPUID reports a hypervisor, so bare metal never sees a write to port `0x510`), so the harness sets options on the unmodified ISO; parsed in the portable half with host tests; DESIGN §3.2 lists every option, and later phases add theirs to the same parser
- [ ] `ktest=<glob>` runs only the matching in-guest tests and `ktest_repeat=<n>` runs them `n` times in one boot (a test that cannot run twice says so in the registry and runs once), set by `VIBEOS_KTEST` and `VIBEOS_KTEST_REPEAT`, with the harness reporting passes out of runs and multiplying its boot timeout by the repeat count; `loglevel=` sets the §5.5 runtime level at boot
- [ ] `cargo-fuzz` targets for every byte-slice parser, the ELF header parser included, on the weekly job; each crash becomes a replayed regression (T4)
- [ ] a crafted FAT BPB fails the mount with `Corrupt`: `fat::parse_bpb` computes `data_lba` (`rsvd + num_fats * fatsz`) with `checked_mul` and `checked_add` and returns `Corrupt` on overflow, and returns `Corrupt` for a cluster count above `0x0FFF_FFF5`, which also bounds `fat_loc`'s `clu * 4`; today the sum panics the kernel under the dev profile's overflow checks and wraps in a release build. Host tests mount a BPB with FATSz32 `0x8000_0000` and two FATs and one with `0xFFFF_FFFF` and one FAT, and each mount returns `Corrupt` (F064)
- [ ] a scheduled macOS CI job running `make check`, and `make test` with Homebrew's QEMU and firmware (I1, F079)
- [ ] OVMF and the aarch64 edk2 firmware located by one probe mechanism with one variable per architecture; the probe also finds the variable-store template beside each code image (I1)
- [ ] UEFI e2e boots the firmware as read-only code on pflash unit 0 (`-drive if=pflash,format=raw,unit=0,readonly=on`) with a per-run copy of its variable-store template on unit 1, never through `-bios`, so Homebrew's code-only `edk2-x86_64-code.fd` (0x37C000 bytes, which `-bios` refuses because the size is not a multiple of 64 KiB) boots (F079)
- [ ] `test-e2e-uefi` checks for the firmware and starts the harness in one shell line: a missing image prints the skip and exits 0 outside CI and fails when `CI` is set; today the check's `exit 0` ends only its own recipe line, and the harness runs anyway (I1, F079)
- [ ] when QEMU exits before the first marker, the harness prints QEMU's exit status and stderr instead of only `missing marker 'serial_online'` (F079)
- [ ] the `/bin/tests` stall after `user: dup ok` (sometimes after `user: pid 3 killed SIGSEGV`) root-caused and fixed, with a regression test in the cheapest tier that catches it; it leaves the e2e boot short of `shell ready` and hangs the in-guest boot under the periodic LAPIC, and has been retried since PR #75 (F021)
- [ ] the `-smp 4` `ipi: ack timeout` panic root-caused and fixed, with a regression test in the cheapest tier that catches it; it is retried on the first boot and on the persist reboot, `wait_acks` raises it after about 1 s without an ack, and the one CI run the review traced hit it while the registry held IF off on the BSP (F011, F075)
- [ ] the `-smp 4` `msix_cpu: ap counter` assertion failure root-caused and fixed, with a regression test in the cheapest tier that catches it (F021)
- [ ] the `-smp 2` `per_cpu_bsp: ready_head should be empty` assertion deleted with its harness retry: `ready_head` is a snapshot from the BSP's last relink, not an invariant (F074)
- [ ] the harness retries nothing: after the four boxes above, `_retry_hang`, `retryable_ktest_failure`, `silent_user_syscalls_hang`, and the retry loop in `_ktest_boot` are deleted, and none of those names appears in `tests/harness/`. Today `_ktest_boot` retries any `timed out`, `_retry_hang` any e2e `timed out` or `no shell ready`, and `retryable_ktest_failure` any `-smp 4` panic whose tail holds `ipi: ack timeout`, the `wait_acks` frame, or a banner on a `ktest: ok` line; a retry it grants after a `FAIL` line cannot pass, because each attempt reuses the disk the failed attempt wrote (F021, F076)
- [ ] DESIGN §8.6 states that a failure traced to a QEMU bug is not retried either: it is reported upstream, the report is linked from DESIGN §8.6, and the kernel or the harness's QEMU command line works around it
- [ ] until those retries are gone, every retry the harness takes is written to the job summary with its failure line, so a green run that retried is visible
- [ ] `reap_many_via_idle` compares frame counts against a quiescent baseline: a shared setup warms KVA past one coalesce and fills the Dead TCB slots before the first frame-accounting test, and it passes with `VIBEOS_KTEST=reap_many_via_idle VIBEOS_KTEST_REPEAT=20` at `-smp 2`; today fresh KVA page-table pages and new `Tcb` boxes move the global `free_frames()` count, and it fails `make test` at random with no retry (F074)
- [ ] e2e asserts `/bin/tests`' result on the production path: `user: tests ok` is a contract marker before `shell ready` in every e2e variant that reaches the shell, and `user: tests fail` is a failure signature (F073)
- [ ] `/sbin/init` passes a status pointer to `wait4` and, when `/bin/tests` exits nonzero or on a signal, prints `init: /bin/tests exited <status>` on fd 2, a registered failure signature, as §10.5's Rust `/sbin/init` does; today `/sbin/init` passes a NULL status to `wait4` and forks `/bin/sh` regardless (F073)
- [ ] the vibefs QEMU-kill test can fail. The guest prints `wr N` before each commit; on any `sync_fs` error it prints a registered failure line and halts. Each round kills QEMU at a random commit count between 1 and 200 and fails unless the harness killed QEMU itself. After `fsck-vibefs` reports `errors 0 warnings 0`, the harness reads `/crash/w` from the image with a hostlib tool over `vibeos-core`'s vibefs and requires the content of iteration N or N-1 for the last `wr N` it saw. This box lands with §10.11's commit-leak fixes (F080)
- [ ] the second ksyms link leaves `.text` where the first put it: each `KERNEL_VARIANT` recipe reruns `gen_ksyms.py` on the final ELF, in the dev and `CARGO_PROFILE=release` profiles, and fails when the table differs from the one it linked (F084)
- [ ] the panic e2e requires the symbolized `panic_fmt` frame, which the panic ISO names `core::str::count::do_count_chars+0xc` today (F084)
- [ ] the in-guest clock tests can fail: `now_us_monotonic` and `now_us_under_yields` read the clock through a `kernel_tests` hook that skips `monotonic_max`'s `LAST_NS` clamp, compare each read with an independently published timestamp within a tolerance the test states, and run a reader thread on every CPU that calls `yield_now` while the timer fires; a planted tear (a hook that skips the seqlock retry) fails them (F100)
- [ ] assertions that cannot fail replaced: `dma::publish_uses_release_not_only_compiler_fence` is deleted, since a single-threaded test cannot see a missing fence (§11.7's virtio litmus test covers publication); `split_wrap_and_sim_device` drives `should_kick` with explicit `avail_event` values and asserts both outcomes; `block_vblk_rw` drops its `barrier()` assertion and its empty `!has_flush()` branch (F120)
- [ ] the expect-panic e2e matches markers only against lines before the first panic signature, counts dump banners rather than signature lines, and waits for the guest's panic exit instead of killing QEMU at `panic: halted`: `isa-debug-exit` status 35, or QEMU's `GUEST_PANICKED` event once §10.7's `pvpanic` box replaces `isa-debug-exit` (F141)
- [ ] the harness's SMP check counts exactly `N-1` `smp: ap online` lines at `-smp N` and fails on an extra one (F141)
- [ ] the marker-order unit tests drive `run_qemu_and_check` through a fake line source, and `check_markers_in_order`, which no runner calls, is deleted (F141)
- [ ] in-guest tests check what their names claim: `lock_spins` prints its counters without counting as a pass, `int3_roundtrip` asserts a flag its `#BP` handler set, `rtc_offset` asserts a plausible RTC year and checks `deadline_after`'s result, `mmio_uc_flags` restores the physmap leaf it made UC, and `preempt_two_threads` pins both workers to one AP with `spawn_on` (F142)
- [ ] the host test `switch_context_roundtrip` runs the kernel's switch asm on x86_64 hosts, with `cli` and `sti` supplied by a macro, in place of the `#[cfg(test)]` copy in `thread.rs`, which has neither and which aarch64 hosts compile out (F142)
- [ ] the `mkfs-vibefs`/`fsck-vibefs` and `$(INITRD)` rules depend on `$(KERNEL_SRCS)` and `Cargo.lock`, so an edit to `src/fs/`, `src/part.rs`, or `src/lib.rs` rebuilds the host tools and the initrd; today the first lists only `src/vibefs.rs` and the second only `src/fat.rs` (F143)
- [ ] `CARGO_PROFILE=release` is built and booted: the nightly job runs `make CARGO_PROFILE=release test-e2e test-kernel`; no workflow builds that profile today (F137)
- [ ] invariants that must hold in release builds are `assert!`, as DESIGN §9.4 requires: `BootCell::set`'s set-twice check (`cell.rs`), `pop_head` on an empty order (`pmm.rs`), and the `carve` bounds (`heap.rs`) are `debug_assert!` today (F041, F137)
- [ ] a contract line or log record reaches serial whole: `Serial::write_fmt` formats the line, newline included, into a stack buffer and writes it under one TX hold, and `log_fmt` appends its newline to its `StackBuf` and sends one `try_write_bytes`; an in-guest test at `-smp 4` has every AP print formatted lines in a loop while the BSP prints 1,000 numbered markers, and `run_ktest.py` finds all 1,000 whole (F138)
- [ ] kernel lines are framed (DESIGN §2.6). Every line the kernel writes to its console UART starts with the byte 0x1E, `marker!` and `klog!` lines, ktest verdicts, and the panic dump included; a `\r`, `\n`, or 0x1E inside a line prints as `?`; the console `write` that user descriptors reach prints a 0x1E in user bytes as `?`; and before a framed line the kernel writes a newline when the last byte on the UART was user output that did not end one. The framebuffer console never draws the frame. Every driver in `tests/harness/` reads a line as the kernel's only when its first byte is 0x1E, strips the frame, and matches contract markers, ktest verdicts, and `PANIC_SIGNATURES` only on such lines, and the lines user programs print (the `utest_*` protocol, `user: tests ok` and `user: tests fail`, `init: /bin/tests exited`, `/bin/sh`'s `shell ready`, and the console-input replies) only on unframed ones; before the first framed line it fails fast on Limine's panic line, which cannot be framed. Unit tests drive the matcher with framed and unframed copies of each kind. A `/bin/tests` case writes 0x1E, a `vibeOS: ktest: FAIL forged` line, `panicked at`, and `#GP` to fd 1 and fd 2; the run stays green, and the harness finds those lines unframed, with `?` where each 0x1E was. `docs/LINUX.md` gains a deliberate-difference row for the escape, since Linux passes user bytes to its console unchanged. This box lands after the box above, which writes each line whole, and before §10.5's runtime box, whose fd-2 panic message is the first user line a kernel signature would match. DESIGN §2.6, §2.7's I28 row, §8.2, §8.3, and §9.7 drop their not-yet-enforced notes in the same commit

### 10.3 Portable core and the architecture seam
- [ ] `switch_context` and the DMA fences out of the portable half; hostlib as a workspace member; host tests on any OS (A2 landed the crate and host tests; `thread.rs` still carries `cfg(target_arch)` and `global_asm!`)
- [ ] `dma::dma_mb`, a full barrier (`mfence` on x86_64) beside `dma_wmb` and `dma_rmb`, runs in `SplitQueue::should_kick` before its `avail_event` or `used.flags` load, which follows `publish`'s `avail.idx` store, and in `SplitQueue::get_used` after its `used_event` store, before the next `used.idx` load, as virtio 1.2 §2.7.13.4.1 requires and Linux's `virtio_mb` and `virtio_store_mb` provide. Today only `dma_wmb` (`fence(Release)` + `sfence`), which orders no later load, separates the `avail.idx` store from that load, and `get_used` issues no barrier after its `used_event` store, so under `VIRTIO_F_EVENT_IDX` one lost kick or interrupt stops a queue for good. A host test through the stub `arch` records ring stores, ring loads, and barriers and finds `dma_mb` between each such store and load; §11.7's litmus set covers both pairs on a weakly ordered CPU; DESIGN §4.7, §9.3, and §10.4 state the barrier as built in the same commit (F016)
- [ ] an `arch` module boundary with one trait per concern: page table format and flags, context switch, interrupt controller and vector map, timer and cycle counter, atomics and barriers, MMIO accessors, per-CPU base register, syscall entry and user context, user-memory access, cache maintenance, the boot handshake; each port implements them on one zero-sized type, and portable code takes it as a type parameter, never `dyn` (DESIGN §11.1)
- [ ] every x86 assumption outside `arch/` found by grep for `asm!`, `x86`, and CR and MSR names, then moved or fenced; the audit checked in as `docs/ARCH.md`, which maps each seam trait to its implementing module
- [ ] the seam proven by a host build of the portable core against a stub `arch`, which is the cheapest second architecture there is
- [ ] one directory per subsystem, portable and hardware halves adjacent; DESIGN §1.3 rewritten to match the tree (A1)
- [ ] `fs/mod.rs`, `vibefs.rs`, `fat.rs`, and `ktest.rs` split by responsibility; a file-size guard in `make check` (Q5)
- [ ] the nine two-way module dependencies broken; `serial` has a raw layer with no upward calls (A4)
- [x] one `BootCell` and one `IrqCell`; no `static mut`; no `&'static mut` accessors (Q3; the asm-owned setjmp buffer in `arch/catch.rs` is the documented exception)
- [ ] `BootCell<T>` is `Sync` only when `T: Send + Sync` and `Send` only when `T: Send`, and `IrqCell<T>` is `Send` and `Sync` only when `T: Send`, as `OnceLock` and `Mutex` are; the three statics that then fail to build (`per_cpu_init::CPUS`, `proc_init::TABLE`, `smp_init::STARTING`) get an `unsafe impl` on the contained type whose `// SAFETY:` line names the invariant (F017)
- [ ] `IrqCell::force_unlock` and `log_init::with_logger_unlocked` become `unsafe fn` (F017)
- [ ] `scripts/check_cells.py` fails on an `unsafe impl` of `Send` or `Sync` for a generic type with no bound on its parameter (F017)
- [ ] the lock guards have `std::sync::MutexGuard`'s auto traits: `InterruptGuard` and `BlockingMutexGuard` each hold a `PhantomData<*const ()>`, which makes them and `SpinMutexGuard`, which holds an `InterruptGuard`, `!Send`, and `SpinMutexGuard` and `BlockingMutexGuard` are `Sync` only when `T: Sync`; a compile-time assertion in the kernel binary fails the build if a guard becomes `Send` or a guard over a `!Sync` `T` becomes `Sync` (F038)
- [ ] `PerCpu` split into owner-only state and a remote view: the fields other CPUs read (`ticks`, `switches`, the run-queue length, `ready`, `wake_inbox`, `apic_id`) are atomics or set once before `ready`, `per_cpu_init::cpu(id)` returns a view with only those fields, `with_current_switch` ends its `&mut PerCpu` before `switch_context`, and `with_cpu` becomes an `unsafe fn` whose contract is that the target CPU is not running; DESIGN §7.5 states which accessor may alias which (F039)
- [ ] the kernel writes the TSS and an AP's `CpuTables` only through pointers with write provenance: `gdt::bsp_tss_ptr` derives from `addr_of_mut!((*BSP.as_ptr()).tables.tss)`, `smp_init` keeps an AP's `CpuTables` as the pointer `Box::into_raw` returns and frees it with `Box::from_raw`, and the wait-queue pointer that `thread_init`'s unlink writes through derives from a `*mut WaitQueue`, not from `WaitQueue::cookie(&self)` (F089)
- [ ] `DmaBuffer` and `GuardedStack` are move-only handles with private fields that only their allocators build (`DmaBuffer::from_phys` is deleted), so `free_to_buddy`, `dma_init::free`, and `kva_init::free_stack` free only what their caller owns; `compile_fail` doc tests in `vibeos-core` show that a `DmaBuffer` can be neither copied nor built from an address (F018)
- [ ] the buddy hands out an owner token: `Buddy::alloc(order)` and `alloc_constrained` return `Frames` (a base and an order, private fields, neither `Copy` nor `Clone`, `#[must_use]`), `Buddy::free(Frames)` is a safe fn, and `deallocate(PhysAddr, order)` becomes private to `pmm`; `PhysAddr` stays a `Copy` address that owns nothing (DESIGN §4.2). Dropping a `Frames` leaks it, counts it in `meminfo`, and in debug builds panics naming its `#[track_caller]` allocation site; it never frees, since a free on drop would take BUDDY under HEAP, SCHED, or DEVICE. `GuardedStack`, `DmaBuffer`, the `vmap` handle, and heap growth hold their `Frames`; `paging::FrameAlloc` and `addr_space::FrameFree` trade `Frames`; a frame a page-table entry maps is consumed into the entry and taken back only by the page-table code that removes it. It lands with or after the box above. `compile_fail` doc tests in `vibeos-core` show that a `Frames` can be neither copied nor built from a `PhysAddr`, and a host test drops one and finds the leak count at 1 and the frame still allocated; DESIGN §4.2's API block is updated in the same commit (F018)
- [ ] `make test-unit` runs doc tests, which its `cargo test --lib` skips today (F018)
- [ ] `addr_space_init::load_cr3_u64` and `syscall_init::switch_cr3_for` become `unsafe fn` with `# Safety` sections (F018)
- [ ] a `Buddy::new` argument replaces `Buddy::set_hhdm_offset` (F018)
- [ ] `paging_init::current_mapper` returns a guard that holds the PT lock (F018)
- [ ] `addr_space_init::teardown` asserts that no CPU's CR3 (TTBR0 on aarch64) and no TCB's saved root holds the root it frees, and `finish_exit` zeroes the exiting thread's `as_cr3` (`thread_init::set_pid_cr3(tid, 0, 0)`) before `teardown`, as `execve` sets it to the new root before it frees the old one, so the assertion holds on every exit path once §10.6 makes syscall bodies preemptible. The in-guest test `teardown_live_root_asserts` tears down a root that a parked TCB's `as_cr3` still names and hits the assertion under `arch::catch::catch_panic` (F018, F106)
- [ ] `kva_init::vmap` returns a move-only handle that records its base and frame count, and `vunmap` takes that handle, so the span unmapped always equals the span returned to the KVA free list; an in-guest test vmaps 32 frames, vunmaps the handle, and finds all 32 pages unmapped (F018, F107)
- [ ] `unmap_shootdown` asserts `n <= MAX_UNMAP` (32) where it clamps silently today (F107)
- [ ] enabling and disabling interrupts and opening and closing the SMAP window are compiler barriers: `x86::sti`, `x86::cli`, `x86::stac`, `x86::clac`, `InterruptGuard`'s exit `sti`, and the raw `cli` and `sti` in `console_init::wait_key` and `thread_init::halt_if_idle` drop `options(nomem)`, while `sti; hlt` keeps it; `syscall_init::flags_if_on`, whose `pushfq` is declared `nostack`, is replaced by `x86::interrupts_enabled()` (F091)
- [ ] the lock-rank checker catches a spinlock held across a context switch: `thread_init::switch_now` asserts that this CPU's `HELD` rank mask is empty once the `SCHED` guard has dropped; an in-guest test that yields while holding a ranked `SpinMutex` hits that assertion under `arch::catch::catch_panic` (F108)
- [ ] the heap ranks first (DESIGN §2.1): `lock.rs` orders `RANK_HEAP` 1, `RANK_PT` 2, and `RANK_BUDDY` 3, with SCHED, DEVICE, and SERIAL unchanged, so an allocation or a free made while PT, BUDDY, or a later rank is held fails the rank check on every call, not only when it grows the heap. `heap_init::grow_for` still drops the heap lock before it takes PT, and each allocation under PT or BUDDY that the ladder then finds moves before the lock. The host test `ranks_are_the_documented_order` pins the new values, and `pt_then_heap_is_forbidden` and `buddy_then_heap_is_forbidden` replace `heap_then_buddy_is_forbidden`; `heap_init`'s lock-order comments and `ktest`'s spin-count line follow the constants. An in-guest test allocates one byte inside `paging_init::with_pt` under `arch::catch::catch_panic`, finds the rank assertion, and restores the `HELD` mask and IF state the longjmp skips. DESIGN §2.1 and I1 drop their not-yet-enforced notes in the same commit
- [ ] every `IrqCell` static that more than one CPU takes after `smp: done` becomes a `SpinMutex` with a DESIGN §2.1 rank, so the rank checker sees it and its spin services incoming IPIs: `proc_init::TABLE`, `kva_init::KVA`, `work_init::ST`, and `irq_init::IRQ` among them; `log_init::LOG` keeps its unranked TAS (DESIGN §2.5); DESIGN §2.3 lists each `IrqCell` left and why it is CPU-local or boot-only (F108)
- [ ] the rank checker enforces DESIGN §2.3's nesting rule and §2.2's last row: `SpinMutex::lock` and `try_lock` fail the check when this CPU already holds a lock of their rank, and `lock_nested` takes a second lock of a held rank, keeping a per-rank count so the inner release leaves the outer rank held; each same-rank nesting the ladder then finds, the cells the box above converts included, either drops the outer lock first or becomes a `lock_nested` whose comment names its pair order. `lock_enter` and `IrqCell::with` also fail while this CPU runs work that `service_incoming` started or an NMI, `#MC`, or CPL-0 `#DB` body, through a per-CPU depth those paths raise; the check is off once `HALTING` is set, since the panic path is §2.2's stated exception. Host tests in `lock.rs` cover the count and both refusals; in-guest under `arch::catch::catch_panic`, two `RANK_DEVICE` locks nested with `lock` hit the assertion, and the same pair nested with `lock_nested` leaves the outer rank held after the inner release, each test restoring the `HELD` mask and IF state the longjmp skips. DESIGN §2.3 drops its not-yet-enforced note in the same commit (F108)
- [ ] a blocking call from a device top half fails at the call: `park`, `wait_on`, `begin_wait`, and the `from_irq = false` path of `thread_init::schedule_inner` assert `!irq_init::in_hard_irq()`, while `schedule_preempt` from the timer and reschedule IPIs stays legal; an in-guest test finds `in_hard_irq()` true inside a device top half and false in its threaded bottom half (F110)
- [ ] every call that may sleep checks that it may (DESIGN §2.9 rule 4): `park`, `wait_on`, `begin_wait`, `BlockingMutex::lock`, `RwLock`'s read and write locks, and `Semaphore`'s acquire assert in debug builds that IF is on and this CPU's `HELD` rank mask is empty, beside the F110 check in the box above. It lands after §10.2's box that moves the in-guest registry to a thread with IF=1, since the registry holds IF off today (F075). An in-guest test takes a ranked `SpinMutex` and then a `BlockingMutex` under `arch::catch::catch_panic` and finds the assertion (F108)
- [ ] an IF-off tracer in an `irqoff` build variant (§10.2): it reads the §10.3 cycle counter where IF goes from 1 to 0 (spinlock and `IrqCell` acquire, `InterruptGuard`, each entry stub) and where IF returns to 1, and logs each stretch longer than DESIGN §2.9 rule 2's bound with the site that turned IF off, skipping the stretches rule 2 exempts and those a `kernel_tests` test or hook holds on purpose through a guard that marks itself; in this box's commit the in-guest tests that hold IF off on purpose (this section's 50 ms clock test, §10.10's 3 s acknowledgement test, and every stall hook) take that guard. The nightly job runs `make test-kernel` and `make test-e2e` in this build under TCG with `-icount shift=0` and `-smp 1`, where guest time counts instructions (a test that needs a second CPU skips with that reason), and writes each logged site to its job summary and the §10.9 history. A logged site is chunked by a box in the phase that owns its code; one that no chunking can bring under the bound, such as a hardware sequence that must run with IF off, moves the number in DESIGN §2.9 with its measurement beside it or joins rule 2's exemptions. The §10.1 KVM leg runs the same build and records per site the longest stretch and the 99th percentile, with no threshold, since its host can deschedule a vCPU mid-stretch. This box lands after §10.2's box that moves the in-guest registry to IF=1
- [ ] the IDT error-code split has one source: a `const` assertion in `arch/idt.rs` fails the build unless `install_defaults`'s error-code and no-error lists partition 0..=255 as `vectors::pushes_error_code` does, `idt::set_handler` asserts `!pushes_error_code(vec)`, and `vectors::NAMED` includes `MC` (0x12) (F093)
- [ ] `now_ns` is derived from the TSC alone when CPUID reports an invariant TSC and the §10.7 warp test saw no backward step: `(rdtsc - tsc0) * 1_000_000 / tsc_per_ms` in `u128` (DESIGN §6.4 option 2), still clamped by `monotonic_max`, so an IF-off window longer than 1 ms or a late TSC-deadline rearm no longer loses time and the tick drives scheduling only; otherwise the tick-counted clock stays; an in-guest test that skips without an invariant TSC, and so runs on the §10.1 KVM leg, holds IF off for 50 ms of HPET main-counter time and finds `now_ns` advanced by 50 ms within 1% (F027)
- [x] `BootInfo` captured once at entry, the only consumer of Limine responses (D3)
- [ ] DESIGN.md split per its own §1.4 rule: invariants and pitfalls as their own files, the boot order as a table not prose (DOC2); its `scripts/doc_refs.py` resolves `DESIGN §x.y` and `ROADMAP §x.y` citations in docs, source comments, and scripts, and every bare `§x.y` in ROADMAP.md, against the headings, and runs in `make check`

### 10.4 Tables, VFS, errors
- [ ] threads, processes, descriptors, inodes, dentries, files, mounts, and regions allocated at init from a `limits` module; a cap is a constant, not a type (D1)
- [ ] the cross-CPU wake inbox holds any `ThreadId` up to the `limits` thread count (a bitmap sized from the limits, or an MPSC list), and the KVA free-list node pool no longer caps freed thread stacks at 128 ranges; today's `u64` inbox caps ids at 64 and drops the rest silently (D1)
- [ ] a full thread table is an error, not a panic: `sys_fork` returns `EAGAIN` when §10.10's fallible `spawn_inner` finds no free slot, AP bring-up leaves a CPU offline and logs it when its idle thread or per-CPU workers find none, and `adopt_ap_idle` drops an unplaced `Box<Tcb>` only after `SCHED` is released; an in-guest test fills the thread table with parked kernel threads, then `fork` from a user program returns `-EAGAIN` (F037)
- [ ] a pid is not a table index (DESIGN §2.11 rule 4): pids and tids come from one allocator, a process's pid being its first thread's tid, in increasing order up to `pid_max` (Linux's default, 32,768, until §23.4 makes it `kernel/pid_max`), wrap to 2 skipping ids in use, and map to their table slot through a lookup, so a reaped pid is not reused until the counter comes round. Today `proc_init::alloc_pid` hands out the lowest free slot index, so the next `fork` reuses the pid of the process just reaped, and a stale `kill` or `wait4` reaches the new process (F127). A host test allocates and frees 100 pids and sees none reused before the wrap; another marks an id as still carried by a process group after its process is reaped, and the allocator skips it at the wrap
- [ ] `MAX_ELF` removed: the loader maps segments from the file instead of reading the whole binary into a bounded buffer
- [ ] allocation on a path untrusted input reaches is fallible (DESIGN §4.4): `vibeos-core` gains `kalloc` (`TryBox`, `TryVec`, `TryString`, `TryArc`, and an ordered map), whose growing operations return `Result` and which builds on stable Rust; `clippy.toml`'s `disallowed-types` denies `alloc`'s `Box`, `Vec`, `String`, `Arc`, `Rc`, and `alloc::collections` types in both crates outside `kalloc`, and `disallowed-macros` denies `vec!` and `format!`; each boot-time or invariant-bounded site that keeps an `alloc` type carries an `#[allow]` whose comment names its bound, and every other failure returns `ENOMEM`; host tests fail each `kalloc` operation through a counting allocator and find the collection unchanged. A `kernel_tests` hook fails every heap allocation after a chosen count; armed at every count from zero to the number of allocations each call makes, `fork`, `execve`, and `open` from a user program each return `ENOMEM` and the kernel stays up, and a failed `execve` returns to the caller's old image, as DESIGN §4.4's point-of-no-return rule requires (F010)
- [ ] counted objects follow DESIGN §2.11 rules 3 and 6: a `TryArc` allocation carries a deferred-release link and a release function beside its count, and a drop that takes the count to zero where rule 6 forbids a release in place links the object onto this CPU's deferred-release list, allocating nothing, and queues one work item on the ordinary workqueue that releases the list with IF=1. The drop reads IF, the `HELD` mask, and whether the thread is a no-reclaim thread (here the threaded-IRQ bottom half and a worker running a softirq-equivalent item; §12.5 and §12.6 add theirs) through a hook the kernel installs at boot, and `put_deferred` is the explicit form. `sync` and `sync_init` gain rule 3's operation gate: `enter` fails once the gate is killed, `exit` leaves it, and `kill` marks it dead, wakes the wait queues its owner names, and sleeps until no operation is inside. Host tests: a drop to zero in a simulated atomic context lands on the list and the worker releases it; an `enter` after `kill` fails, and `kill` returns only after the last `exit`. §10.8 loom models cover `TryArc`'s count and the gate; a variant with a `Relaxed` decrement, and a variant whose `enter` reads the dead mark before it counts itself in, must each fail. In-guest, a `kernel_tests` hook drops the last reference to a `TryArc` inside a `SpinMutex` section, and the worker releases the object after the section ends. It lands after the `kalloc` box above and §10.8's atomics-seam box
- [ ] FAT and vibefs behind `InodeOps`; `Vfs` owns inodes, dentries, mounts, and files and nothing backend-specific (A3)
- [ ] the VFS lock guards the namespace tables only (DESIGN §2.1): A3's `BlockingMutex` is held for a lookup, an insert, or a removal, and for a namespace change's or a dentry-cache miss's directory I/O, never across a backend's data I/O, a wait, or a user copy; `read`, `write`, `truncate`, and `getdents64` run on counted inode and file references with it dropped, and backend operations never receive `&Vfs`. Each FAT and vibefs volume's busy flag becomes a level-4 `BlockingMutex` that owns the volume, is taken with a plain `lock()`, and never fails an operation for contention; `grab`'s 1,000,000-yield `EIO` and `drop_slot`'s force-clear are deleted in both backends. It lands with A3's lock-kind step, after the FAT stack-frame box below. In-guest: a read of a `vda` file held at a `kernel_tests` hook delays neither an `open` of a tmpfs path nor a tmpfs `read` on another CPU; two kernel threads, each started with `spawn`'s 16 KiB stack, read and write FAT files on `vda` through `Vfs` while a `kernel_tests` hook delays each block request by 2 s, and neither gets `EIO`. DESIGN §2.1 drops its not-yet-enforced note, and SYSCALL.md §2 and §2.1 their busy-volume `EIO`, in the same commit (A3, F060)
- [ ] every path syscall resolves through `Vfs`: `file_init::walk_abs`, `file_init::vol_parent`, and the `route` and `routed_rest` functions of `fat_init` and `vibefs_init` are deleted, so a process reaches kernfs; a user test opens `/dev/null`, `/dev/zero`, and `/dev/random` and reads or writes each, and an in-guest test then finds no `null` entry in the FAT initrd's `/dev` directory; it lands after §10.12's `/dev/random` box and after this section's dentry-cache (F065) and tmpfs (F066) boxes, since routing path syscalls through `Vfs` makes both reachable from ring 3 (A3, F086)
- [ ] a relative path resolves against the calling process's `Proc::cwd`; `file_init::CWD`, which the kernel shell's `cd` writes and every process reads, is deleted, and the kernel shell keeps its own directory; an in-guest test sets the kernel shell's directory to `/vibe`, then runs a user program whose relative `open` resolves from `/` (F057, F086)
- [ ] a syscall path is canonicalized before lookup: `//` and `.` collapse, `..` resolves and stops at `/`, and a case-insensitive FAT lookup returns the dentry that carries a mount, so `/./vibe/f`, `//vibe/f`, `/dev/../vibe/f`, and `/VIBE/f` all open the vibefs file `/vibe/f`; an in-guest test opens each (F056)
- [ ] the open-file table never writes `refs` or `used` back from a snapshot: `put_file` writes back only the fields its operation changed, `refs` and `used` change only under the table lock, in `addref` and `close`, and each slot carries a generation that every fid lookup and write-back checks, so a write-back to a slot freed and reused in the meantime fails with `EBADF`; a non-`O_EXCL` create that finds the name already present opens it instead of returning `EEXIST`. In-guest, with a `kernel_tests` hook that yields inside `write` between the snapshot and its write-back: a parent writes through an inherited descriptor while its child closes and reopens descriptors 1,000 times, the parent's writes never reach another file, and at the end each slot's `refs` equals its holders (F055)
- [ ] one refcounted in-core FAT inode per file, keyed by its dirent location, owns the first cluster, the size, and the dirent slot, and an open file holds only a reference, its offset, and its flags; FAT read, write, and truncate update the inode under the volume lock, rename re-keys it, and `free_chain` runs at the last close of an unlinked file; FAT and vibefs `O_APPEND` and `SEEK_END` read the inode's size; a host test writes through two descriptors on one empty file, truncates through one and extends through the other, and the host-test `fsck` helper (`fsck.fat -n`) passes (F013)
- [ ] the dentry cache never resolves through a reused slot: a directory dentry stays pinned while a child dentry names it as parent, `dcache_find` matches the mount as well as the parent and name, `Vfs::mount` pins the mountpoint before `dentry_force_alloc`, and `Vfs::umount` makes every busy check before it changes state; host tests put `/a` under eviction pressure and still find the mount on `/a/m`, and find that `stat("/c/x")` never returns `/a/x` (F065)
- [ ] `umount` keeps volume state consistent: `Vfs::umount` dispatches through the mount's filesystem operations, never by trying each backend in turn; after the busy checks the box above orders first, it returns `EBUSY` while an open file, a working directory, or a root is reached through the mount or one of its submounts; it syncs a vibefs volume; and it releases the volume instance D2 registers only after the unmount succeeds. In-guest, on a thread started with `spawn`'s 16 KiB stack: a failed unmount leaves the mount usable, and a remount after a clean unmount succeeds (F060)
- [ ] tmpfs keeps written data when a file's extent moves: `tmp_ensure` writes the old run's dirty `tmp_cache` pages back to `tmp_back` before it copies the run, and `Cache::invalidate`, which drops pages without writeback, runs only for runs that unlink or shrink frees, as its contract comment then says; a host test writes file A, creates B beside it, grows A by write and by truncate, and reads A's first bytes back (F066)
- [ ] `open` reserves the descriptor and the open-file slot before it creates or truncates anything; an in-guest test calls `open(O_TRUNC)` with the open-file table full and gets `ENFILE`, and the file's size is unchanged (F057)
- [ ] `rm -r` in the `kernel_shell` build walks with a bounded explicit stack, builds child paths with a length check that returns `NameTooLong`, and removes every entry, not the first 16; an in-guest test runs it on a 20-entry, 8-deep tree (F126)
- [ ] `ls` in the `kernel_shell` build lists a FAT or vibefs subdirectory: `vfs_ls_snap` treats `NotSupp` as not kernfs; an in-guest test runs `ls /etc` (F126)
- [ ] `mount` in the `kernel_shell` build reuses the pinned dentry of an already-mounted path in `vfs_attach`; an in-guest test runs 64 `mount`s of one path from a thread started with `spawn`'s 16 KiB stack (F126)
- [ ] `unlink_path` drops the name from the resolved parent, not `/` (F126)
- [ ] FAT timestamps: `fat_datetime` counts years from 1980 with the Gregorian leap rule, `fat_to_unix` is its inverse, and a host test round-trips every day from 1980 through 2107; `FatVol::now` and `Vfs::now` follow `time_init::unix_time_s()` instead of staying 0 (F123)
- [ ] driver and volume state as instances owned by their device's registry entry (DESIGN §12.1), so a second disk is a second instance and no driver module keeps a list of them (D2)
- [ ] a disk-backed FAT mount fits a 16 KiB kernel stack: `fat_init::mount_dev` builds `FatVol` in its slot instead of moving it by value, and per-call cluster buffers move into per-volume storage under the volume lock; an in-guest test mounts a FAT image on `vda` from a thread started with `spawn`'s 16 KiB stack (F058)
- [ ] one registry of counted block-device handles (`BlockRef`, DESIGN §12.1), keyed by name and by a 64-bit id never reused within a boot, partitions included as children of their disk: the block cache, `part_init`, FAT, vibefs, and devfs block nodes hold a `BlockRef` instead of matching `DEV_RAM0` and `DEV_VDA`, the cache keys its pages by that id, a device's name is an owned array of up to 32 bytes (Linux's `DISK_NAME_LEN`) instead of a `&'static str`, and vibefs no longer treats an unknown device id as `vda`; a host test unregisters a partition while a `BlockRef` to it is held, and I/O through the handle returns `Gone` (DESIGN §2.11 rule 3's gate), a lookup by name no longer finds it, and a new registration gets a new id; an in-guest test mounts a FAT32 partition of `vda` by its partition name and reads `/dev/vdap1` through `Vfs`, from a thread started with `spawn`'s 16 KiB stack, never the registry's; it lands with or after the FAT stack-frame box above; it lands after the counted-object box above (D2, F081)
- [ ] one `KError` with `From` for every module error and the Linux errno mapping in one table; syscall dispatch returns `Result<usize, KError>`; the table emits the errno table in `docs/SYSCALL.md` §2 through the §10.5 generator, and `make check` fails if the checked-in copy differs (E2)
- [ ] the E2 table gives each condition Linux's errno: `FsError::NoSpace` splits into a full volume (`ENOSPC`, 28), a full system-wide open-file table (`ENFILE`, 23), and FAT's 4 GiB file limit (`EFBIG`, 27); `Corrupt` from FAT and vibefs maps to `EIO`; `lseek` on the console returns `ESPIPE` (29); `Loop`, `NotEmpty`, and `NotSupp` map to `ELOOP`, `ENOTEMPTY`, and `EOPNOTSUPP`; a host test per variant replaces `fat.rs`'s `error_strings` test, which pins `Corrupt` to `Inval` (E2, F052, F057, F083)
- [ ] the syscall layer's own checks return Linux's errno: `read` on an `O_WRONLY` descriptor and `write` on an `O_RDONLY` one return `EBADF`, `dup` with a full descriptor table returns `EMFILE`, and `open` and `execve` pass a path's bytes to the filesystem and accept an argument of any bytes but NUL instead of returning `EINVAL` for one that is not UTF-8; an in-guest test checks each case (E2)
- [ ] `fat::FatError` and `vibefs::Error`, two copies of one 11-variant enum, merge into one filesystem error type with one `From` into `KError` (E2, F083)

### 10.5 User runtime
- [ ] a `no_std` Rust crate under `user/` as a workspace member, statically linked, with `_start`, argument and environment parsing, and a panic reported on fd 2 and turned into a non-zero exit
- [ ] built for the bare target as a non-PIE `ET_EXEC` linked below 2 GiB, through the static relocation model the `[target.x86_64-unknown-none]` rustflags already set (the built-in spec defaults to static-PIE, which the loader refuses until §13.10), plus `-C code-model=small` for the user crate alone, since the built-in spec's model is `kernel` and those rustflags are shared with the kernel; the prebuilt `core` keeps the `kernel` model, which also resolves below 2 GiB, so `-Zbuild-std` stays deleted (B2); Rust code with `std` targets `*-unknown-linux-musl` (§24.3)
- [ ] syscall stubs generated from one table shared with the kernel, with a number column per architecture (§11.6) and an argument order where Linux's differs by architecture (raw `clone`: arm64 swaps `tls` and `ctid`), so numbers, arities, and argument positions cannot drift; the same generator emits `docs/SYSCALL.md` §3
- [ ] syscall dispatch indexes the generated syscall table by number, and before the handler runs, the dispatcher range-checks every pointer argument the row declares (a buffer with its length argument, a fixed-size value, a C string, or a string vector); the rows for `open` (path), `execve` (path, argv, envp), and `wait4` (status) declare theirs, where `syscall.rs` gives all three a `ptr_mask` of 0 today; an in-guest test passes an unmapped and a kernel-half pointer in each declared pointer argument and gets `-EFAULT` (F150)
- [ ] `execve` copies the caller's `envp` strings onto the new image's initial stack after `argv`, so the crate's environment parsing sees them; today `sys_execve` discards `envp` and `user_init::fill_stack` passes an empty one; a `/bin/tests` case execs a program with the environment `K=v`, and the program exits 0 only when its environment holds `K=v`
- [ ] `elf::parse` host tests also cover checked-in static binaries linked by `ld.lld` and by GNU `ld`; today every host-test image comes from the test helper `build_elf`
- [ ] `brk` and anonymous `mmap`/`munmap`, eagerly backed for now; Phase 12 makes them lazy without changing the interface
- [ ] `getdents64`, `fstat`, and `nanosleep`, which `ls` and `sleep` below need; the rest of the floor stays in §13.9
- [ ] `reboot` (power off and restart), so the user `/bin/sh` keeps the `poweroff` and `reboot` the Phase 5 kernel shell had; x86_64 uses the ACPI and reset paths, and §11.4 puts PSCI behind the same call
- [ ] an allocator over `brk`, so `alloc` works in userspace
- [ ] `utest_ok` / `utest_fail` / `utest_skip` on serial, asserted by `tests/harness` like the `ktest_*` protocol in `make test-e2e`, `make test-e2e-uefi`, `make test-e2e-pit`, and `make test-e2e-highmem`, where `/bin/tests` runs as a forked child of `/sbin/init`; a failing user test fails `make test` (F073)
- [ ] `/sbin/init`, `/bin/sh`, `/bin/tests`, and `/hello` rewritten in the crate; the assembly sources and `mkuserelf.py` deleted
- [ ] a signal cannot kill or stop pid 1: `sys_kill` to pid 1 delivers only signals init has a handler for, as Linux does, so none until §13.8's `rt_sigaction`; `/bin/tests` sends `SIGKILL` to pid 1, and the boot still reaches `shell ready` (F068)
- [ ] pid 1's exit panics the kernel with a line naming its status (F068)
- [ ] a host-tested `vibeos-core` helper picks an orphan's reaper: pid 1 while its slot is live or stopped, and otherwise none, in which case the orphan's zombie is freed when it exits (F068)
- [ ] the Rust `/sbin/init` checks every `fork`, `execve`, and `wait4` result: a nonzero `/bin/tests` status prints `init: /bin/tests exited <status>` on fd 2, which the harness fails the run on; on `ECHILD` or a failed `/bin/sh` start it writes a diagnostic to fd 2, yields, and starts `/bin/sh` again, and after three failed starts it exits non-zero, which the kernel turns into the pid 1 panic above; a harness case boots an initrd without `/bin/sh` and expects the diagnostic and that panic (F073, F128)
- [ ] `/bin/tests` provokes every errno SYSCALL.md lists for each syscall and asserts it; each pointer argument of `read`, `write`, `open`, `execve` (path, argv, envp), `wait4` (status), `psinfo` (syscall 500), and the §10.5 additions gets a kernel-half address, an unmapped page, a range crossing `USER_MAP_END` and one crossing `USER_END`, a read-only page as a destination, a huge length, and NULL where SYSCALL.md does not allow it, and each returns `-EFAULT`; the `USER_MAP_END` and read-only cases need §10.6's `USER_MAP_END` and accessor boxes, so this box lands after them (F077)
- [ ] `/bin/tests` covers the process lifecycle: the fork bomb fails unless `fork` returns `-EAGAIN` at the `limits` process count; a grandchild reads `getppid() == 1` after its parent exits; a three-step exec chain ends with the parent's `wait4` returning the last program's exit status; `wait4` on one pid blocks until that child exits although a sibling exited first, and `wait4` on that zombie sibling then returns its status at once; `SIGKILL` ends a child, and `SIGSTOP` then `SIGCONT` stops and resumes one (F077)
- [ ] `/bin/sh` runs programs from `PATH` with `fork`, `execve`, and `wait4`, reports their exit status, and has `poweroff`, `reboot`, and the assembly shell's `ps` built-in, over syscall 500 until §13.9 moves `ps` to `procfs`; pipes and job control are §13.7
- [ ] `ls`, `cat`, `echo`, `grep`, `wc`, `true`, `false`, `sleep`, `yes`, and `cmp`, each a few dozen lines, because the Phase 13 gate is a pipeline of them
- [ ] the initrd is sized by `mkinitrd` from its contents plus fixed free space for the write tests, and loaded as a Limine module on x86_64 instead of a 64 KiB image embedded by `build.rs`; `INITRD_BYTES` and its size check are deleted, and DESIGN's boot order and `build.rs` pitfall are updated to match
- [ ] nothing in the crate names an architecture outside one `arch` module, so Phase 11 builds it for aarch64 by adding a directory

### 10.6 Entry paths and user memory
The kernel's entry paths and its user-memory copies predate copy-on-write, demand paging, and signal
return. Each item here is a live bug or a trap for Phase 12 and 13.

- [ ] user-VA accessors replace the physmap copy for syscalls. `copy_from_user` and `copy_to_user` keep the range check (canonical, above the null guard, inside the user half, no overflow), because inside `stac`/`clac` the MMU still allows supervisor access to kernel pages. They then dereference the user address inside `stac`/`clac`, so `CR0.WP` faults a write to a read-only page and a not-present page faults. A fault inside an accessor becomes `EFAULT` through an exception-table fixup, which replaces the page-table pre-walk. Today `write_bytes` writes through the physmap and ignores `WRITABLE`, so `read`, the `wait4` status pointer, and `psinfo` can write into the caller's read-only and executable pages, which Phase 12's COW would turn into cross-process corruption. DESIGN §5.1 and SYSCALL.md §5 are rewritten in the same commit to describe the accessors as built (F023)
- [ ] no padding reaches user memory (DESIGN §2.4): the typed form of the copy-out accessor above takes only a type bounded by `zerocopy`'s `IntoBytes`, whose derive refuses at compile time a type with padding or uninitialized bytes, and its byte form takes a `&[u8]`; `zerocopy` enters with the note AGENTS.md requires. A uapi struct whose Linux layout has an implicit hole declares it as an explicit field that the kernel zeroes. A `compile_fail` doctest copies out a `#[repr(C)]` struct with a hole and fails to build. This box lands with the accessor box or after it, in its own commit
- [ ] one named fill API for writing an address space that is not running: ELF segments, zero fill, TLS, the exec stack, and `fork`'s copy until §12.3. It writes frames through the physmap and syscall code cannot reach it. It holds `PT` only to look up or install mappings, for at most one leaf table (512 entries) at a time, and copies or zeroes page contents with `PT` dropped, with IF on between chunks (DESIGN §2.9 rule 2), so a large `fork` copy does not hold off every other CPU's page-table work and its own tick for the whole copy. §12.2 teaches it to fault pages in with write intent
- [ ] exec returns `ENOMEM` for an image whose page-rounded `PT_LOAD` and `PT_TLS` sizes together exceed a cap in the §10.4 `limits` module that is larger than the default guest's 128 MiB, so the under-cap case below exists. Today `elf::parse` checks `p_memsz` only against the user half, so one `execve` drains the buddy allocator under the PT lock with IF off. `AddressSpace::map_anon` frees what it mapped when a frame allocation fails, as the `map_page` error path does, and the loader releases the PT lock between bounded chunks. In-guest, in the default 128 MiB guest: an ELF with a 64 GiB `p_memsz` and one under the cap but larger than free memory each get `ENOMEM`, and a `fork` after them succeeds (F009)
- [ ] the ELF loader maps `PT_LOAD` segments that share a page, which a §10.5 user-crate binary has when it is linked with no page alignment between `.text` and `.data`: it merges their page-rounded intervals, maps each page once with the union of the segments' write and execute flags, zero-fills it once, then copies each segment's file bytes. Today `map_loads` takes `AsError::Overlap` as success and zeroes the earlier segment's bytes. Host tests cover an RX and an RW segment in one page and a pair whose second segment runs past the shared page; an in-guest test execs such a pair (F031)
- [ ] every interrupt and exception entry clears `RFLAGS.AC` (`clac` when SMAP is live) before anything else. Interrupt gates do not clear AC, ring 3 can set it with `popf`, and an IRQ or `#PF` inside an accessor would otherwise run its handler with SMAP off; `iretq` restores the interrupted value. An in-guest test opens the accessor window, takes an exception, and has a test hook in the handler make a stray access to a user page, which must fault; an x86_64-only variant does the same after a user program sets AC with `popf`; the test hook compiles only under `kernel_tests` (F088, F146)
- [ ] the top user page is never mappable: one `USER_MAP_END` (`0x0000_7FFF_FFFF_F000`) used by the ELF loader and the address-space range checks, so a `syscall` in the last page cannot return to a non-canonical RIP. The exit path sends a non-canonical saved RIP to `SIGSEGV` before `swapgs`, never to `iretq`. A `#GP`, `#NP`, or `#SS` whose RIP is a labeled user-return `iretq` (the syscall slow path, `vibeos_iret_user`, `vibeos_iret_user_full`, and the interrupt exit) is recognized, handled on the kernel GS, and turned into `SIGSEGV` for the process instead of `exception_halt`. DESIGN §4.1 and SYSCALL.md §1 updated in the same commit. An in-guest test execs an ELF whose last `PT_LOAD` page is the one below `USER_END` and ends in a `syscall`, and gets `ENOEXEC`; a `kernel_tests` hook that writes a non-canonical RIP into a syscall's saved frame finds the process killed by `SIGSEGV` and the kernel running. Both run on the KVM leg too, since TCG's `helper_ret_protected` skips `iretq`'s canonical check (F007)
- [ ] the IST vectors (NMI, `#MC`, `#DB`, `#DF`) decide whether to `swapgs` from the sign of `GS_BASE`, not from CS.RPL. The syscall entry before its `swapgs` and the exit's `swapgs; sysretq` and `swapgs; iretq` run at CPL 0 with the user GS base loaded. An in-guest test puts hardware execute breakpoints on those instructions, and the `#DB` handler finds its `PerCpu`. The sign check holds until §18.3 lets userspace write a kernel-half GS base (F007)
- [ ] the 512 MiB low identity window torn down after `smp: done`, so VA 0 faults in kernel mode as it does in user mode. Only the `0x8000` trampoline page stays: 4 KiB, read-only, executable, not global, with the trampoline GDT's accessed bits preset so the AP never writes it. The BSP writes the blob and parameter block through the physmap. Before the teardown, `smp_init` asserts that the bootstrap thread's stack lies outside the window. The teardown flushes global TLB entries on every CPU. An in-guest test finds that a kernel read of VA 0 faults. DESIGN §4.1 and §4.3 updated in the same commit (F085)
- [ ] the syscall exit path runs with IF=0 from the dispatcher's return to `sysretq` or `iretq`: `cli` directly after `call vibeos_syscall_stub`, before the `gs:[retval]` store. Today a console `read` returns with IF=1 from `wait_key`'s `sti; hlt`, so a preemption before `sysretq` lets another process's syscall overwrite `gs:[retval]`, and an interrupt after `mov rsp, [rsp]` pushes its frame at the user's RSP at CPL 0. `console_init::wait_key` returns with the IF state it was entered with, a debug-build check before each exit `swapgs` faults if IF is set, and SYSCALL.md §1 states the rule. An in-guest test blocks a user `read` on fd 0 in `wait_key` until another thread queues a key, with the check live (F001)
- [ ] stop and continue cannot lose a wakeup: `apply_pending` checks `state == Stopped` and calls `begin_wait` on `stop_wq` inside one `with_sched(|s| TABLE.with(..))` section, then schedules and loops until the process is continued or killed, as `sys_wait4` does, and the unlocked `TABLE.as_ptr()` read goes. Today the Stopped store and the wait are two SCHED sections, and a `SIGCONT` between them wakes an empty queue. In `make test-kernel-smp4`, a kernel thread on CPU 1 sends `SIGSTOP` then `SIGCONT` 10,000 times to a process on CPU 0 that calls `getpid` in a loop, and after each pair the process runs again (F033)
- [ ] syscall bodies run with IF=1 (DESIGN §2.9 rule 3): after `swapgs` the entry stub copies the user RSP from `PerCpu.syscall_scratch` into its frame on the thread's kernel stack and then runs `sti`, and the handler of a fault or trap taken at CPL 3 runs its body with IF=1 once its frame is saved; the exit `cli` of the syscall-exit box above closes the window. Today FMASK clears IF at `syscall` and nothing sets it again, so a long syscall holds off the tick and every shootdown acknowledgement on its CPU (F011, F044). DESIGN §5.10's syscall row and SYSCALL.md §1 describe the entry as built in the same commit. It lands after §10.3's teardown box (F106), §10.4's open-file table box (F055), and the stop-wait box above (F033), whose findings preemptible bodies make reachable. In-guest, in `make test-kernel-smp4`: a `kernel_tests` hook makes one `getpid` of a test program spin for 50 ms of TSC time on CPU 0; a kernel thread on CPU 1 unmaps a KVA range during the spin, and the unmap returns before the `getpid` does; and with a CPU-bound thread ready on CPU 0, CPU 0's context-switch count advances during the spin; a `kernel_tests` hook at the top of the `#PF` body of a CPL-3 fault yields to a ready thread of another process that then faults at a different unmapped address, and each process's `user:` kill line names its own `cr2=` (DESIGN §5.10 rule 9) (F011)
- [ ] `enter_user` and `enter_user_full` execute `cli` before they load the user data selector into GS and write `GS_BASE` and `FS_BASE`, and `iretq` restores the user IF from the saved RFLAGS. Today a spawned or forked process reaches `enter_user_full` with IF=1, so an interrupt after `mov gs` reads `gs:[0]` at VA 0 at CPL 0 and halts the kernel. The selector loads, MSR writes, and `iretq` run as one asm block after the `cli`. An in-guest test forks and exits 1,000 children on CPU 0 while CPU 1 sends CPU 0 reschedule IPIs, and the kernel stays up (F006)
- [ ] every IDT vector enters through a stub that `arch/idt.rs` generates from one vector table, and the stub makes the `swapgs` decision and saves CR2 for `#PF` and DR6 for `#DB` into the frame before the body runs (DESIGN §5.10 rule 9), so the AC clear and the IST sign check above each live in one place. `idt::set_handler` takes a body function, not an `extern "x86-interrupt"` handler. Today the pool stubs `irq_init::device_irq::<N>` (vectors 0x31 to 0x7F) and `kbd_init`'s `kbd_ioapic` and `kbd_pic` skip `gs_enter`, so an interrupt through any of them at CPL 3 reads `gs:[0]` at VA 0 and halts the kernel. `scripts/check_entry.py` in `make check` fails on an `extern "x86-interrupt"` function outside `src/arch/`, and once no such function remains anywhere, `#![feature(abi_x86_interrupt)]` leaves `src/main.rs`. The in-guest test `user_device_irq` runs ring 3 with IF=1 on CPU 0 while CPU 1 sends it a pool vector and the keyboard vector, and `user_ipi` does the same with reschedule IPIs; in both the process and the kernel keep running (F004)
- [ ] a pending signal whose default action terminates or stops the process takes effect on every return to ring 3: the entry stub's exit path checks the current process's pending set before its `iretq` to CPL 3; handler delivery stays in §13.8. Today signals apply only at syscall entry and after the `wait4` sleep, so `SIGKILL` cannot end, and `SIGSTOP` cannot stop, a process that makes no syscalls. It lands after the stop-wait box above. An in-guest test kills one child looping in ring 3, stops and continues another, and reaps both (F033)
- [ ] no fault or trap that ring-3 code raises reaches `exception_halt`: one table in the portable half, which `sig_for_vec` reads, gives each of vectors 0 to 31 its CPL-3 action, and a host test fails on a vector without one. `debug_ex` calls `try_user_fault` for a CPL-3 frame after it moves from the IST stack to the thread's kernel stack, as Linux's `idtentry_mce_db` does, since the kill path can block; the stub saves DR6 into the frame and clears it before the move, and the body reads DR6 only from the frame (DESIGN §5.10 rule 9). `sig_for_vec` maps `#DB` to `SIGTRAP` and `#AC` to `SIGBUS`, and CPU init clears `CR0.AM` on every CPU. Today a user `popf` that sets TF, or an `int1` (`0xF1`), halts every CPU. In-guest, a child that sets TF with `popf` and a child that executes `int1` each end with `SIGTRAP` in their `wait4` status; DESIGN §2.5 and §5.2 drop their F005 not-yet-enforced notes in the same commit (F005)
- [ ] the `#BP` gate is DPL 3 and `sig_for_vec` maps `#BP` to `SIGTRAP`, so a user `int3` gets `SIGTRAP`, as on Linux. Today `desc.rs` builds every gate at DPL 0, so a user `int3` raises `#GP` and gets `SIGSEGV`, as DESIGN §5.2's `#BP` row records. An in-guest child that executes `int3` ends with `SIGTRAP` in its `wait4` status (F148)
- [ ] one per-CPU control-register routine, run on the BSP and on every AP, sets `CR0.NE` and `CR0.MP`, clears `CR0.EM`, `CR0.TS`, and `CR0.AM`, and sets `CR4.MCE`, `CR4.OSFXSR`, `CR4.OSXMMEXCPT`, and `CR4.PGE`. Today no CPU sets NE, MCE, or OSXMMEXCPT, so a user x87 error raises the masked IRQ13 and a machine check shuts the CPU down, and only the AP trampoline sets PGE, so the BSP runs with PGE clear, as Limine's CR4 of `0x20` leaves it. The per-CPU CR in-guest test asserts each bit on every CPU. A user x87 divide by zero with the zero-divide exception unmasked ends with `SIGFPE`; on the §10.1 KVM leg, so does an SSE divide by zero with `MXCSR.ZM` clear. DESIGN §5.2's `#MF`, `#XF`, and `#MC` rows then hold (F026, F085)
- [ ] x86_64 user FP state follows the psABI across `fork` and `execve`: `sys_fork` copies the parent's `Tcb.fpu`, which the syscall entry's `fxsave64` filled, into the child before `make_ready`, instead of the boot template `spawn_user` installs; `sys_execve`, after its point of no return and with IF=0, writes an image with FCW `0x037F`, MXCSR `0x1F80`, and ST0-7 and XMM0-15 zero into `Tcb.fpu` and loads it with `fxrstor64`, so a context switch before the syscall exit saves the new image, not the old one. SYSCALL.md §1 and DESIGN §2.7's I25 row and §7.5 drop their F069 notes in the same commit. `/bin/tests` sets MXCSR's rounding bits and `xmm0` before a `fork` and the child reads both back, and an image that `execve` starts reads MXCSR `0x1F80`, FCW `0x037F`, and `xmm0` zero (F069)
- [ ] console `write` holds IF=0 only for bounded work (DESIGN §2.9 rule 2): it takes the console lock once per 256-byte chunk, IF is on between chunks because the syscall body runs with IF=1 (the syscall-body box above), and the framebuffer console scrolls at most once per chunk, by min(newlines, rows) rows, from a RAM copy of the text grid, never reading VRAM back; a redraw writes at most 16 KiB of framebuffer per console-lock hold, retaking the lock for the next piece and drawing each piece from the grid as it then stands, and a line written with IF already off updates the grid and leaves the redraw to the next console write whose caller runs with IF on. Today each newline on the last row is a full-framebuffer `memmove` with IF=0, so one `write` stalls CPU 0 without bound. An in-guest test writes 4096 newlines from ring 3 while CPU 1 sees CPU 0's tick count advance (F044)
- [ ] one ring-3 entry model: `/hello` and every in-guest test that enters ring 3 or binds a process run as spawned processes the kernel waits on, and the bound model is deleted: `run_path`, `run_user`, `boot_hello`, `bind_current`, `bind_probe`, `with_user_as`, `unbind_current`, `longjmp_user`, `vibeos_user_setjmp`, `vibeos_user_longjmp`, `Proc.borrowed`, the globals `USER_JMP`, `IN_USER`, `EXIT_STATUS`, and `STDOUT`, and the `p.bound` and `in_user()` branches in `finish_exit` and `SYS_EXIT` dispatch. That removes a `setjmp` called from Rust, a bound `execve` whose address space nothing tears down, and a bound exit that skips `reparent_children`. The one setjmp left is `arch/catch.rs`'s, behind its asm trampoline, and SYSCALL.md §3 drops "bound `run_user`". In-guest, a spawned program that forks and exits without waiting leaves an orphan that §10.5's reaper rule frees when it exits, since a `kernel_tests` build starts no `/sbin/init` (F082, F040, F087, F127)
- [ ] an address space is a two-count object (DESIGN §2.11): `users` counts its process's thread and any pin, a pin is taken only by get-unless-zero, and the last `users` put runs the teardown; the core, a `kalloc` `TryArc` that each region and each `users` holder references, holds the root and, from §12.1 and §12.3, the page-table lock and the CPU set, and freeing it frees the root and runs no teardown, so §10.3's teardown assertion moves to the root's free. Code reaches a space through a scoped guard (`with_current_space(|as| ..)`), never a `&'static AddressSpace`: `current_space`, `space_of`, `current_as`, `peek_user_as`, `set_user_as`, `CURRENT_AS`, and the pid-0 fallback are deleted, and the box above has already deleted `bind_current` and `Proc.borrowed`. It lands after the user-VA accessor box, the box above, and §10.4's `kalloc` box. In-guest, a `kernel_tests` hook pins a process's space from a kernel thread across that process's exit, the frames return to the buddy when the pin drops and not before, and a pin taken after the exit fails and finds the frames already back (F019)
- [ ] boot runs on a guarded stack: the kernel includes Limine's stack size request (256 KiB), and once KVA is up the bootstrap thread moves to a KVA stack with a guard page. Today `_start` and all of boot run on Limine's stack, which is guaranteed only 64 KiB, has no guard page, and lies in bootloader-reclaimable memory, so an overflow writes that memory silently. An in-guest test finds the bootstrap thread's `Tcb.stack` set, and DESIGN §2.4 and §9.2's guard-page rule then holds for every kernel stack (F072)

### 10.7 Forensics and tracing
A hang or a panic under QEMU explains itself from the host, without a rerun. It lands early: the §10.2
retry root-causing and Phases 11 to 13 debug their hangs with it.

- [ ] on a timeout, or on a panic once the kernel's dump has ended, the harness takes a guest core over the QEMU monitor before it stops QEMU: `dump-guest-memory -p`, an ELF core with the kernel's virtual mappings and one register note per CPU, which `gdb` and the §10.1 `make debug` script open (not `-z`, whose kdump format `gdb` cannot read); an x86_64 panic signals the ISA `pvpanic` device with `-action panic=pause`, as aarch64 does (§11.7), instead of `isa-debug-exit`, so QEMU stays up for the dump and the harness then fails the run; CI uploads a failed run's core (compressed with zstd on the host), kernel ELF, and QEMU command line as one artifact; a job in a workflow that names an environment or a secret uploads no core, memory dump, or QEMU command line, and `scripts/check_workflows.py` fails on one that does
- [ ] a hostlib core tool reads a core with the kernel ELF and prints a symbolized frame-pointer backtrace for every CPU, every thread's state and saved context from the TCB table, each CPU's current thread and run queue, and the last 64 records of the §5.5 log ring; the harness prints its report after the serial tail
- [ ] the log ring, TCB, and per-CPU types the tool reads are the portable crate's and `#[repr(C)]` (`IrqCell`, `log::Record`, `Ring`, and `Logger` are not today), with `const` assertions on their sizes and field offsets, outside `cfg(loom)`, that compile into both the kernel and the host tool, so the kernel and the host tool, built on Linux or macOS, cannot disagree, as `mkfs-vibefs` and `fsck-vibefs` share vibefs's format
- [ ] a flight recorder (moved from §19.1): a per-CPU ring of fixed-size records (timestamp, CPU, event, two arguments); each CPU writes only its own ring, with interrupts off for the few stores a record takes, so no lock is taken; the NMI, `#MC`, and `#DB` handlers do not record, since `cli` does not mask them, and each record carries a sequence number so the tool drops a torn last record; it is on in every build, so a hang of the production ISO carries a trace too
- [ ] tracepoints at the boundaries that exist now: syscall entry and exit, scheduler switch and wake, IRQ entry and exit, IPI send and acknowledge, page fault, and block request submit and complete; §19.1 adds the later subsystems and per-tracepoint runtime enable
- [ ] timestamps from the §10.3 cycle counter (the invariant TSC on x86_64, §2.6); each AP measures its TSC against the BSP's at bring-up with a warp test over a shared cache line, and a registered marker reports the largest skew, which DESIGN §6.4 requires before timestamps order a trace; when the TSC is not invariant or the warp test saw any backward step (Linux's `check_tsc_warp` rule), the export orders records within each CPU only and says so, and DESIGN §6.4 gains that rule in the same commit; the core tool converts them to nanoseconds with the calibration and the warp result it reads from the core and exports every CPU's ring as one Chrome trace-event JSON timeline, which Perfetto opens; a host test checks the export's required fields
- [ ] a `hang_test` feature build in which, after `smp: done`, one CPU prints a registered `hang_test: armed` marker, takes a spinlock the others then wait on, and spins with interrupts off; `make test-forensics`, in `make test`, boots it at `-smp 4`, gives up 5 s after the marker rather than at the harness timeout, so its tier stays inside the §10.1 budget, and asserts the report and the export, so the forensics are tested the way the panic path is
- [ ] the panic dump owns COM1: `halt_others` waits up to 100 ms of TSC time for every other online CPU to acknowledge the `0xFE` IPI, then sends NMI to each CPU that has not, and the NMI handler halts without writing once `HALTING` is set, so a CPU spinning with IF=0 in `SpinMutex::lock` or `wait_acks` also stops; only the first CPU into `begin_dump` runs `Serial::init`, after that wait, and a later one halts without writing; once `HALTING` is set, `write_bytes_plain` and `try_write_bytes` write only on the `DUMPING` owner. Today `halt_others` returns without waiting, a CPU spinning with IF=0 never takes the IPI and writes COM1 without the TX lock, and a second CPU into `begin_dump` re-runs `Serial::init` mid-dump. DESIGN §2.5 and §7.6 describe it as built in the same commit. A `panic_test` variant at `-smp 4` panics CPUs 0 and 1 at once while CPU 2 prints a numbered line in a loop with IF=0; the harness finds one panic message, its backtrace, and one `vibeOS: panic: halted` line, no numbered line after the first `vibeOS: panic:` line, and the panic exit that §10.2's expect-panic e2e requires (F135)
- [ ] an exception dump's backtrace starts at the interrupted frame: the §10.6 entry stub passes the interrupted `rbp` to `dump_backtrace`. Today `exception_halt` and `exception_vec` pass their own `rbp`, so for an error-code vector (`#GP`, `#PF`, `#DF`, and the `default_err` handlers) the walk reads the interrupted RAX, which LLVM's x86-interrupt prologue pushes at `[rbp+8]`, as a return address, and the `regs: rbp=` line shows the handler's value. The `gp_test` e2e boot faults in a function that `gp_test_trip` calls, and the harness requires the backtrace to list `gp_test_trip` and then `normal_boot_tail` (F070)
- [ ] after `begin_dump`, every panic-path write to COM1, the `reentered` line included, goes through A4's raw serial layer (§10.3) with IF already off and no `InterruptGuard`. Today those writes take an `InterruptGuard`, so after an `irq nest underflow` panic every write fails the guard's assertion again and the dump prints `reentered` without end. A `panic_test` variant underflows `irq_nest`, and the harness finds the original panic message and the panic exit that §10.2's expect-panic e2e requires (F071)
- [ ] the panic backtrace follows `rbp` only into a known stack: the current thread's KVA stack, the recorded boot-stack bounds, or this CPU's IST and RSP0 stacks. Today `stackish()` also accepts the `ioremap` window and every address below `0x2000_0000`, so a walk can read an MMIO register that clears on read. `vibeos_syscall_entry` zeroes `rbp` before `call vibeos_syscall_stub`, so the chain ends at the syscall boundary instead of following the user's `rbp`. Host tests cover the range check; an in-guest test walks the chain from inside a syscall made with a nonzero user `rbp` and stops at the entry (F139)
- [ ] the blocked-thread sweep reports a timeout that fired at least `OVERDUE_NS` late: `schedule_inner` checks lateness as `pop_expired_into` pops each entry, through a portable pop-and-report function with a host test. Today `overdue(now)` runs after that pop has drained every entry it could return, so the sweep never reports. The harness registers the `sched: overdue tid` marker (F111)
- [ ] dead scheduler and bring-up state deleted: `relink`, which runs under `SCHED` on every schedule, with `Tcb.prev` and `Tcb.next`, which nothing reads, and `PerCpu.ready_head`, which only the `per_cpu_bsp` in-guest test reads; `PerCpu.tsc_per_ms`, which only the `per_cpu_identity` in-guest test reads; and the trampoline's `PARAM_IDT` with `smp::pack_idtr`, which `smp_init` writes and the trampoline never reads. §10.2 deletes `per_cpu_bsp`'s `ready_head` assertion and its harness retry, since `ready_head` is a stale snapshot, not an invariant (F074, F111)
- [ ] syscall tracing can be switched on: `strace=1` on the §10.2 command line calls `syscall_init::set_trace(true)`, which nothing calls today, and SYSCALL.md §6 names the option. An e2e boot with `strace=1` finds a `user: syscall write` line for the first user `write` (F150)
- [ ] a syscall count per process: the per-TCB `syscall_count`, summed over a process's threads, is reported through syscall 500 and the `/bin/sh` `ps` built-in, which §13.9 moves to `procfs`. Today nothing reads the per-TCB count. The global `SYSCALLS` counter, bumped on every entry and never read, is deleted. An in-guest test finds a process's count grown by the number of syscalls it made (F150)

### 10.8 Models and proofs
Tests sample interleavings and inputs. These cover all of them up to a stated bound, on the code the
kernel links rather than a transcription of it. They run on the host and land before Phase 11 runs the
kernel on a weakly ordered CPU.

- [ ] portable code reaches atomics, fences, and the spin-loop hint only through the §10.3 atomics seam, which is `loom`'s under `cfg(loom)`. Because `loom`'s atomics have no `const fn new`, the seam also re-exports `core`'s atomics for `static`s (`entropy`'s and `paging`'s today), which stay `core`'s under `cfg(loom)` and out of every model, and a constructor built from seam atomics (`TickClock`, `IrqCell`, `BootCell`) is `const fn` only outside `cfg(loom)`. `scripts/check_atomics.py` in `make check` fails on `core::sync::atomic` or `core::hint::spin_loop` anywhere else in the portable crate outside tests; `loom` enters as a `cfg(loom)` dev-dependency listed in the §10.9 policy
- [ ] the §10.4 wake inbox's push and drain (in `ipi_init.rs` today) and `IrqCell`, the log ring's lock (in `cell.rs`, which the portable crate compiles only for tests), move into the portable half beside the §2.7 seqlock (`TickClock`), taking the interrupt guard and the CPU id through the §10.3 seam, so each model runs the kernel's code; the host owner token becomes per thread (loom's `thread_local!` under `cfg(loom)`), since today every host caller is owner 1 and a contended lock reads as re-entry
- [ ] loom models: the seqlock against a writer that publishes an independent timestamp, so a torn read fails; the wake inbox with pushes from three CPUs against a drain, so no `ThreadId` is lost or delivered twice; the log ring's writers against a `dmesg` reader. Each model has a variant with one ordering weakened or one step moved, run as a test that passes only when loom finds the failure
- [ ] `TickClock::write` bumps `seq` to odd with `fetch_add(1, Relaxed)` followed by `fence(Release)` before the payload stores, since the CPU 0 tick is its only writer, and its comment names the reader's `fence(Acquire)` that the fence pairs with. Today's `fetch_add(1, AcqRel)` orders only earlier accesses, so the C11 model lets a reader accept a new tick with an old TSC; that sequence is the seqlock model's weakened variant (F098)
- [ ] Kani proofs, each harness stating its bound: the buddy allocator on a 16-frame arena with orders 0 to 4, where every sequence of up to six allocate and free calls keeps allocations disjoint and aligned, the free count exact, and freed buddies merged; the §10.6 user-range check, as a pure function of `(addr, len)`, accepts exactly the non-empty ranges that do not overflow and lie wholly between the null guard and `USER_MAP_END`, and an empty range at any address below `USER_MAP_END`, the null page included, as Linux's `read(fd, NULL, 0)` needs, for every pair of 64-bit values
- [ ] `Buddy::order_for` returns `None` for a size above `PAGE_SIZE << MAX_ORDER` before it rounds. Today `next_power_of_two` overflows above 2^63: a release build returns `Some(0)` for `(1 << 63) + 1`, and a dev build panics. Host tests cover `u64::MAX` and `(1 << 63) + 1`, and a Kani harness proves that for every 64-bit `(bytes, align)` it returns `None` or an order whose block is at least `bytes` long and `align`-aligned (F103)
- [ ] Miri runs the portable crate's host tests; a test Miri cannot run is skipped under `cfg(miri)` with its reason, and each finding becomes a regression test; `miri` joins the `rust-toolchain.toml` components, so a C1 bump picks a nightly that ships it
- [ ] `make models` runs the loom models and the Kani proofs on Linux and on the Apple Silicon dev host, with `kani-verifier` at a pinned version, since Kani brings its own compiler; the nightly job runs it and Miri, and the scheduled macOS job (I1) runs `make models`
- [ ] DESIGN §8 gains a models-and-proofs tier and its routing rule: a protocol whose correctness depends on an interleaving gets a loom model, not only a stress test

### 10.9 Engineering system
Gate lines, CI timings, and dependencies as data a script reads.

- [ ] a gate map from Phase 10 on: `tests/gates/phase-<N>.toml` gives each exit-gate line of phase N, keyed by its text, the entries that prove it, each a local command, a scheduled CI job on GitHub-hosted runners that must be green on the gated commit, read through `gh`, or a dev-host record; a line with several entries, such as one per architecture or accelerator, passes only when all of them pass; each later phase adds its map in the slice that closes its gate, and Phases 0 to 9 get none
- [ ] `make gate PHASE=N` checks every entry, prints pass or fail per gate line, and fails when a gate line other than the tag has no entry; no entry runs `make gate` itself, so the entry for a line that names it runs that line's other checks; the maintainer runs it before tagging, and `release.yml`'s `build` job runs it at the release tag's commit for the phase that tag closes, and nothing is signed or published when it fails; `scripts/check_gates.py` in `make check` fails when an entry's text matches no gate line in this file, or a job entry names a workflow whose `runs-on` has a `self-hosted` label
- [ ] CI history: when a `ci` run completes, a `workflow_run` job reads its jobs and steps from the Actions API and commits one JSON record (commit, event, conclusion, per-job and per-step wall time) to an orphan `ci-history` branch, which outlives the 90-day limit on Actions logs and artifacts; the job has `contents: write` only, checks out only `ci-history`, runs no code from the triggering commit, never puts run fields (which a fork's pull request sets) into a shell line, writes one file per run id, and retries its push after a rebase, so concurrent runs lose no record; later lines add workflows and fields to the record; `scripts/ci_history.py` prints any step's series and fails when a `ci` run on `main` since the history landed has no record, and gains modes that gate-map entries run as local commands, such as §21.1's `--nested`, which needs a passing leg of each vendor's path within the nightly job's last 7 runs; §10.1's push-to-green numbers are read from it
- [ ] dev-host records, for a gate line or the part of one that runs under HVF, since no hosted CI runner can run an HVF guest: on the Apple Silicon dev host, `make gate PHASE=N RECORD=1` runs each record entry's command in a clean checkout of the gated commit and writes one JSON file per commit and entry (commit, the fixed host label `dev-host` and the Mac model, macOS and QEMU versions, command, the numbers the line measures, pass or fail) to `ci-history`, and never the machine's hostname, a user name, or a path under the home directory, since `ci-history` is public, retrying its push after a rebase; everywhere else, `release.yml` included, `make gate` runs no record entry's command and passes the entry only when `ci-history` holds a passing record for it at the gated commit. A job entry passes on a green run of its workflow at the gated commit from any trigger; when there is none, the maintainer starts one before tagging with `gh workflow run` on a branch at that commit, so every workflow a gate entry names has a `workflow_dispatch` trigger; `release.yml` reads runs and starts none
- [ ] `deny.toml`, and `cargo deny check licenses bans sources` in `make check` (skipped with a hint when `cargo-deny` is not installed, and installed at a pinned version in the `check` job): licenses from an allowlist compatible with the tree's MIT license, crates.io as the only source, and a `[bans]` allow list naming every crate in the graph, so a pull request that adds a dependency fails until it names the crate there, which turns AGENTS.md's dependency note into a check; `cargo deny check advisories` on the nightly job, since it fetches the RustSec database
- [x] `scripts/check_review_refs.py`, which `make check` runs, fails when a finding id in [reviews/KERNEL_REVIEW.md](reviews/KERNEL_REVIEW.md) is cited by no line in this file or a cited id names no finding; its `--closed` mode also fails while a CRITICAL or HIGH finding without a LATENT tag is cited by an open box in Phases 0 to 10; `tests/harness/test_review_refs.py` tests it
- [ ] `scripts/check_ticks.py`: on every pull request, CI lists each line of this file that the pull request changes from `- [ ]` to `- [x]` and fails unless the commit that ticks it carries a `Proves:` trailer for it that names a test, `make` target, marker, or script existing in the tree at the pull request's head; `make check` runs it against `origin/main` when that ref exists; `tests/harness/test_ticks.py` covers the diff parsing and the trailer matching

### 10.10 Lifetimes and liveness
A completion's publishing store is its last access to the waiter. Deferred reclaim frees an object only
after the CPU that owns it has switched away from it. A TLB shootdown waits for every CPU's ack and never panics.

- [ ] `IoWaiter::finish` makes its `done` store its last access to the waiter: under `SCHED` it runs `wake_all` on the waiter's queue, then stores `done`. Storing `done` under `SCHED` before `wake_all` is not enough, because `wait()` returns through the lock-free `poll()` and the waiter lives on the submitter's stack. DESIGN §10.1 and a DESIGN §9.4 pitfall state the rule. An in-guest test stalls the completer at a `kernel_tests` hook just before it takes `SCHED`, and 10,000 ramdisk requests whose submitters return at once complete with no fault and no hung submitter (F002)
- [ ] a dead thread's kernel stack is freed only by the CPU that ran it, after `switch_context` has moved that CPU off it: `thread_exit` parks the stack in a per-CPU slot that the next switch tail on that CPU frees, as Linux's `finish_task_switch` does, and no other CPU's `reap_zombies` touches it; DESIGN §4.5 and §9.2 say so. An in-guest test in `make test-kernel-smp4` exits 10,000 threads on CPU 0 while CPUs 1 to 3 loop through schedule tails, with no fault and the free frame count back at its baseline (F012)
- [ ] every switch tail frees the per-CPU dead-stack slot, the preempt (`from_irq`) tail included, so the slot holds at most one stack, and the global 8-entry `DEFERRED` list and its `kva: deferred free list full` panic are deleted. The in-guest test `exit_burst` exits 16 user processes back to back on one CPU, each switching to a sibling resumed from timer preemption, and the kernel stays up (F010)
- [ ] `spawn_inner` reuses a `Dead` TCB slot only when that thread's `on_cpu` flag, which its CPU clears after `switch_context`, is clear, and `thread_exit` stores `Dead` under `SCHED`. Today the scan skips only the local CPU's current and idle threads, so a spawn on another CPU can rewrite a TCB that is still switching out. An in-guest test in `make test-kernel-smp4` spawns threads on CPUs 1 to 3 while threads exit on CPU 0, and every spawned thread runs its entry exactly once (F012)
- [ ] `spawn_inner` returns `Result` instead of calling `expect("thread stack")`; when the stack allocation fails, `sys_fork` and `spawn_elf` release the pid slot and the cloned address space and return `ENOMEM`. An in-guest test drains the buddy allocator below the 4 frames of one kernel stack, then a kernel-thread spawn returns `Err`; `fork_oom` has a `kernel_tests` hook fail the kernel-stack allocation of a `fork`, whose `clone_full` runs first, and gets `ENOMEM` with the free-frame count back at its baseline; the kernel stays up (F010)
- [ ] `wait_acks` never panics: after 1 s without every ack it keeps waiting and logs, at most once a second, a line naming the CPUs that have not acked, because `shootdown_va` and `call_mask` callers free frames as soon as it returns; a CPU that never acks becomes a hang that the §10.7 forensics report. An in-guest test in `make test-kernel-smp4` holds IF off on one CPU for 3 s of TSC time while another CPU unmaps a KVA range, and both finish (F011)
- [ ] a console `write` of any length acks shootdowns while it runs, because its body runs with IF=1 and holds the console lock one 256-byte chunk at a time (§10.6). An in-guest test in `make test-kernel-smp4` writes 1 MiB to the console on one CPU while another CPU unmaps a KVA range, and the unmap returns before the write does (F011)

### 10.11 Storage integrity
A filesystem or block operation that fails or is refused leaves the free count, the directory entries,
and the data checksums as it found them. The kernel writes to a disk only for a mounted filesystem or a write to its device node, and a block fence
completes only after every request submitted before it.

- [ ] the production kernel writes to a disk only for a mounted filesystem or a write to its device node: `stamp_vda_gpt` and its call in `part_init::init` build only with `kernel_tests`, and that build stamps only when LBA 0 to 33 and the last 33 sectors read back as zeros, never after a read error. `make test-e2e` boots the production ISO once with a 1 MiB `mkfs-vibefs` image on `vda` and once with a 1 MiB image whose only non-zero bytes are `0x55AA` at offset 510 (the entry-less MBR that `part` also finds on a whole-disk FAT32 volume), and compares each image's SHA-256 before and after. DESIGN §3.3 and §10.5 say in the same commit that only a `kernel_tests` build stamps a GPT, and only on an all-zero `vda` (F003)
- [ ] a virtio-blk `Barrier` completes only after every request submitted before it has completed, and a `Flush` goes to the device only after every earlier write has completed (virtio 1.2 section 5.2.6.2); requests submitted after a fence wait until it completes, and `cache_init::flush` waits for the writes the writeback thread already has in flight. A host test on the block `Queue` holds one write in flight and finds a `Flush` submitted after it undispatched until that write completes (F043)
- [ ] the block queue never merges or reorders across a fence: `try_merge` refuses a merge across any queued fence, not only the lowest, `first_fence_seq` returns an `Option` instead of the `u32::MAX` sentinel, sequence numbers are `u64`, and `pick()` never dispatches a write ahead of or together with an older overlapping write. A host test on the `Queue` submits a flush, a write to LBA 100, a flush, and a write to LBA 101, and sees four requests dispatched in that order (F043)
- [ ] virtio-blk negotiates `F_RO`: a write to a read-only device fails at once with a new `BlockError::ReadOnly`, with no retry, and the device keeps serving reads. The `version1_required_and_no_ro` host test is inverted to expect `F_RO`, and `make test-kernel` gains a boot, limited by `ktest=` to one test, whose `vda` is an unpartitioned `readonly=on` image: one write gets `ReadOnly` and a read after it succeeds (F046)
- [ ] a virtio-blk request that exhausts its 3 retries fails alone: `finish` calls `fail_rest` only when the device sets `DEVICE_NEEDS_RESET`, and DESIGN §10.3 states the per-request policy in the same commit. `make test-kernel` gains a boot, limited by `ktest=` to one test, whose `vda` has a QEMU `blkdebug` read error on one sector: that sector's read fails and a read of any other sector succeeds (F046)
- [ ] the virtio-blk probe fails on a device queue size below 3, the descriptor count of one read or write chain, instead of binding a queue on which every request stays `Full`; a host test covers queue sizes 1 and 2 (F046)
- [ ] a vibefs file ends at or below byte `2^44 - 4096`, so its last block index is at most `u32::MAX - 1`: `Vol::write` refuses a write past it with a new `FileTooBig` error that maps to `EFBIG` (27) in `src/syscall.rs` and SYSCALL.md §2, and `file_init::seek` refuses a vibefs offset past it with `EINVAL`; VIBEFS.md §3 states the limit. Host tests write at `2^44 - 4096` and at `2^44` and get `FileTooBig` with the file unchanged; on `/vibe`, an in-guest test gets `EFBIG` from a `write` after `lseek` to `2^44 - 4096` and `EINVAL` from `lseek` to `2^44` (F008)
- [ ] vibefs block arithmetic cannot overflow or truncate: `map_block`, `split_replace_extent`, and `truncate` use checked `u64` arithmetic, and vibefs `Node.size` and the in-core inode size that §10.4's `SEEK_END` and `O_APPEND` read are `u64`, so both hold above 4 GiB. An in-guest test writes one byte at offset 5 GiB of a `/vibe` file, and `lseek(fd, 0, SEEK_END)` returns 5 GiB + 1 (F008)
- [ ] vibefs `commit` calls `mark_meta` on every `DIR_LEAF` and `DIR_INT` block it writes, so the next commit drops them. It counts those blocks against `MAX_META` (48) before the super write and fails with the old super still live, so `mark_meta` cannot fail after the new super is on disk. A host test keeps the free count constant across 200 commits of one file on a 64-block volume and across a remount (F014)
- [ ] vibefs `commit` writes an alloc-map block that already carries this commit's decrements for `old_meta` and the drop list, and applies the same decrements in memory after the super flush; the old super still names the old alloc block, so a crash before the super write loses nothing. A host test runs 64 sessions of mount, 300-byte overwrite, sync, and unmount on a 64-block image, and free space and `fsck-vibefs` warnings stay constant. The F049 note in VIBEFS.md §6 is removed in the same commit (F049)
- [ ] a vibefs write that cannot add an extent leaks nothing: `Vol::write` checks extent-slot capacity, a split included, before `alloc_block`, releases the new block through `pending_drop` on any later error, and returns a short count with `size` updated for the chunks already written; `truncate` propagates `free_inode_data` errors instead of discarding them. A host test retries a write at offset 16 KiB 100 times, and free space is unchanged (F051)
- [ ] vibefs never rewrites corrupt data under a fresh checksum: the overwrite path in `Vol::write` propagates `check_extent`'s error and re-reads the target block after the check, a split or partial truncate recomputes the CRC of each extent it changes, and `Vol::read` verifies an extent before it copies. One host test flips one data byte, then overwrites one byte of that block and gets `Corrupt` from the write and from every later read; another writes block 0 of a 2-block extent and finds block 1 unchanged and every CRC valid (F063)
- [ ] `vibefs::fsck`, which `fsck-vibefs` runs, adds the VIBEFS.md §11 checks it skips: kind and mode sanity, the inline flag against a size of at most 128, no duplicate names in a directory, regular-file nlink against its dirent count, and the bitmap as well as the refcounts against reachability; it also reports an inode not reachable from the root. A host test plants each defect, a directory moved into its own subtree included, and fsck reports each one; another fills one directory with 57 entries, so the `DIR_INT` path is walked (F059, F067)
- [ ] a FAT extend that runs out of clusters leaks nothing: `ensure_size` compares `self.free` with the clusters it needs before it allocates any, and on a later error frees the clusters this call linked and restores the old end-of-chain and `*first`. A host test writes far past EOF on an empty file of a nearly full volume, and the free count and the dirent are unchanged after sync and remount (F052)
- [ ] FAT `dir_reserve` counts the free run that reaches the first `0x00` entry, plus every entry after it to the end of the chain, as free, extends a directory only when that total is shorter than the name needs, and checks the size bound before `alloc_clu`, not after. A host test keeps `free_bytes()` constant across six lowercase creates and a rename in one directory (F053)
- [ ] FAT long names round-trip UTF-8: `fill_lfn` encodes UTF-8 as UTF-16 with `utf16_len` counting code units, `take_lfn` decodes UTF-16 back to UTF-8, and `check_name` rejects `"`, `*`, `:`, `<`, `>`, `?`, `\`, and `|`. Host tests create `café`, look it up, and find exactly one `café` entry in the listing, and `caf??` does not open it (F054)
- [ ] FAT and vibefs `rename` lose nothing for any argument pair: when the destination resolves to the source dirent (same `dir_clu` and `dir_off`, as a case-only FAT rename does), FAT skips the unlink and writes the new name before it deletes the old one; moving a directory into its own subtree returns `EINVAL` on both; and a moved FAT directory's `..` names its new parent (cluster 0 for the root). Host tests rename `a` to `A`, `hello.txt` to `Hello.txt`, and `p` to `p/c/q` (F059)

### 10.12 Drivers and devices
A driver maps only the ranges it holds a claim for, none inside a RAM-typed range of the boot memory
map, and it turns on its own device's decode and bus mastering (DESIGN §12.3). A partition table
yields a child device for each entry it lists, read from the sector that holds it.
`/dev/random` returns only bytes a hardware source produced, each byte to one reader.

- [ ] a device's resources are claimed before they are mapped (DESIGN §12.3): `Registry::claim` checks a memory BAR against every other claim and against the RAM-typed ranges of the boot memory map (usable, bootloader-reclaimable, executable and modules, ACPI reclaimable, and ACPI NVS) under `REG`, and returns a `BarClaim` that is not `Copy` (AGENTS.md rule 6), which the device's registry entry holds. BARs are mapped only in `probe`, through `pci_init::map_bar(&BarClaim)`: `pci_init` no longer maps BARs at enumeration, the MSI-X table is reached through the claim of the BAR its BIR names, the class-03 write-back case is deleted since an unclaimed BAR is never mapped, and `remove` unmaps a BAR and then drops its claim. `Device` is not `Copy`: `probe` receives a `DevRef`, the registry entry records the device's parent bridge or root port and its DESIGN §12.1 state in place of `bound`, one lock per device serializes `probe` and `remove`, and `dev_init::bind_all` no longer writes a probe copy back. The `src/dev.rs` module doc names `dev_init::bind_all` as the kernel's binder, and DESIGN §3.3 says a BAR is mapped by the driver that claims it, in the same commit. An in-guest test finds each bound device's memory BARs in the claims table, a second `Registry::claim` of one refused with `Already`, and a claim over usable RAM refused (F115)
- [ ] `pci_init::map_mmio` refuses a range that overlaps a RAM-typed range of the boot memory map (usable, bootloader-reclaimable, executable and modules, ACPI reclaimable, and ACPI NVS) before any UC patch or `ioremap`, behind the claim check above, and returns `None` on a `patch_physmap_uc` error instead of discarding it, until §11.2 deletes `patch_physmap_uc`. An in-guest test passes `map_mmio` a range inside usable RAM and gets `None`, with that physmap leaf still write-back (F115)
- [ ] a driver turns on its own device (DESIGN §12.3): it sets memory decode before it touches the device and bus mastering after it resets it, and neither `dev_init::bind_all` nor `irq_init`'s MSI and MSI-X setup writes `COMMAND`. A probe that fails after the device holds queue addresses writes device status 0, polls until it reads 0 within DESIGN §9.6's bound, and clears `COMMAND.MASTER` before it frees queue and data memory. It lands with §10.4's D2 rewrite of virtio-blk, which touches the same probe and failure paths. An in-guest test fails the virtio-blk and virtio-rng probes after `QENABLE` through a `kernel_tests` hook and reads status 0 and bus mastering off before the frames return to the buddy allocator (F116)
- [ ] `parse_mbr` copies the four primary entries into a local array before `parse_logical` reuses `sector_buf`, so a primary listed after an extended entry is read from the MBR, not from the last EBR. A host test parses an MBR with an extended entry in slot 1 and a primary in slot 2 and gets the primary's start and length (F117)
- [ ] `part_init` registers a child for every parsed entry up to `MAX_PARTS` (16), not only the 4 names in `NAMES_VDA` and the 5 in `NAMES_RAM`, and `register_table` logs each entry it drops. An in-guest test that registers a table with 6 entries gets 6 child devices (F117)
- [ ] each virtio-rng pool byte reaches one reader: `rng_take` claims and reads pool bytes under the queue lock that `publish_pool` holds, so a refill cannot reset `POOL_POS` between a claim and its read. An in-guest test in `make test-kernel-smp4` has a `kernel_tests` hook fill each refill with a distinct counter pattern, reads while refills land on another CPU, and sees no pattern byte twice (F121, F140)
- [ ] `hw_fill` calls `rng_request` whenever the virtio-rng pool is empty and no request is in flight, not only after a non-empty take, so a zero-length completion cannot stop refills (F121)
- [ ] `RngDriver::probe` refuses a second virtio-rng device once `BOUND` is set, so the second device cannot overwrite `Q` and `ISR_VA` and orphan the first device's queue and vector (F121)
- [ ] `/dev/random` and `/dev/urandom` return only hardware bytes until the §14.7 CSPRNG: a read returns a short count when virtio-rng and `RDRAND` supply less than it asks for and `EAGAIN` when they supply none. The kernfs xorshift fallback `mix_rng`, seeded from `Vfs.now`, which no kernel code sets, is deleted. It lands before the §10.4 `Vfs` routing makes kernfs `/dev` reachable from userspace. A host test with an `entropy::set_hw_fill` hook that supplies 5 of 64 bytes reads 5, and with a hook that supplies none gets `EAGAIN` (F134)

---

## Phase 11: Portability

**Goal.** A second architecture, first class. aarch64 boots the same kernel to the same Phase 9 gate,
under the same harness, with the same markers. x86_64 and aarch64 are peers from here on: every later
phase lands on both, or says which lines are single-architecture and why.

**Unlocks.** Native-speed local runs on Apple Silicon, which is where the kernel is booted by hand.
Proof that the seam from §10.3 is real. Every phase after this one designed for two architectures at
design time rather than retrofitted, which matters most for page faults, signal frames, TLS, and the
syscall ABI.

Why here rather than at the end: Phase 12 (the page fault path, including faults inside the §10.6
accessors) and Phase 13 (signal frames, `sigreturn`, the TLS register) are where the kernel's
architecture-specific surface grows most. Doing them with two ports in the tree forces the seam to be
right where it is designed rather than where it is discovered. The cost is that Phase 12 waits for this
phase. x86_64 came first and stays the reference when the two disagree.

**Exit gate**
- [ ] `make ARCH=aarch64` produces a bootable image; `make ARCH=aarch64 run` boots under `qemu-system-aarch64 -machine virt,acpi=off,gic-version=3` (§11.5, §11.7), with `-accel hvf` on the macOS dev host (GitHub's arm64 runners have no `/dev/kvm`, so the project has no arm64 KVM host)
- [ ] every marker in the shared and aarch64 lists of the [DESIGN.md](DESIGN.md#83-end-to-end) contract, from `serial online` through `shell ready`, appears in order on aarch64, from one harness with an `ARCH` parameter
- [ ] `make test-kernel ARCH=aarch64` passes every in-guest test that is not x86-specific, including the Phase 6 MSI-X tests and the §10.6 user-memory tests, on the default `gic-version=3`, where MSI-X goes through the ITS, and again with `gic-version=2`, where it goes through GICv2m, and the harness reads the verdict from QEMU's exit status and its QMP `GUEST_PANICKED` event under TCG and HVF (§11.7); each x86-specific test is `ktest_skip`ped with a reason naming the x86 feature it needs, and the harness fails if the skipped set differs from the list in `docs/ARCH.md`
- [ ] `/bin/tests` from the Phase 10 user crate passes on aarch64 at EL0, against the arm64 syscall numbers
- [ ] the harness types `echo serial-ok` into the shell on aarch64 through the PL011 and a second command through the QEMU monitor's `sendkey` and virtio-keyboard, and reads both echoes on serial, as the x86 e2e boot does through COM1 and PS/2
- [ ] `-smp 4` under `virt` brings up every core through PSCI, and the TLB shootdown test passes
- [ ] the nightly job, on QEMU 9.0 or later (the first whose `virt` wires the EL2 virtual timer's interrupt), boots aarch64 under TCG with `-machine virt,acpi=off,gic-version=3,virtualization=on -cpu max -smp 4` to `shell ready` with every core online; the exception-level marker reports EL2 with VHE on every core, and the §11.3 timer marker names the EL2 virtual timer. The same boot with machine type `virt-8.2`, whose timer node has no `hyp-virt` interrupt, names the EL2 physical timer
- [ ] CI runs the aarch64 build and tier jobs on every push beside the x86_64 ones (§10.1 budget), under TCG, since GitHub's arm64 runners have no `/dev/kvm`
- [ ] `docs/ARCH.md` maps each §10.3 seam trait to its module in each port and lists every other architecture-specific module; `scripts/check_arch.py` in `make check` fails when a file under either port's `arch/` directory is missing from it, or a module it names does not exist
- [ ] README has an x86_64 quickstart and an aarch64 quickstart, and the aarch64 one runs `make ARCH=aarch64 run` under `-accel hvf` on an Apple Silicon Mac; `scripts/check_readme.py` in `make check`, added in the same commit, fails when either section is missing or a `make` command in them names a target the Makefile does not define
- [ ] no x86 regression: the full x86 ladder stays green on every commit of this phase
- [ ] `make litmus` passes: every `tests/litmus/` test holds under `aarch64.cat` and its x86_64 version under `x86tso.cat`, and each barrier-removed twin allows its bad outcome; host tests show the page-table code refusing every change §11.2's break-before-make rule forbids
- [ ] tag `phase-11` and release `v0.11.0`

### 11.1 Boot
- [ ] Limine on aarch64 over UEFI, so the boot protocol, memory map, HHDM, and framebuffer handshake are shared with x86 rather than reimplemented
- [ ] Limine 12.9 or later, pinned by commit in `setup.sh` and the CI cache keys (12.6 deletes the device-tree `memory@` nodes that contradicted the memory map; 12.9 enters at EL1 on CPUs without VHE). aarch64 requests base revision 6, the only aarch64 revision Limine 11 and later accept; x86_64 may stay at revision 3, with `BootInfo` normalizing the difference. The full x86 ladder stays green across the bump
- [ ] the physmap base is the HHDM offset Limine reports, read once into `BootInfo` and used by every physical-to-virtual translation before and after the kernel's own CR3, on both architectures (DESIGN §4.1); the `paging_init::HHDM_BASE` constant and `boot::capture`'s assertion that Limine's offset equals it are deleted, because the Limine protocol says the HHDM base may vary between boots and must not be assumed. `boot::capture` halts with a registered reason when the physmap's range would overlap a fixed region of DESIGN §4.1; host tests cover an offset that overlaps each region, and a `kernel_tests` build boots with the offset read back from `BootInfo` on both architectures
- [ ] x86_64, before the Limine bump above lands: every CPU clears `CR4.OSXSAVE`, `CR4.PKE`, and `EFER.FFXSR` in `init_cpu` and asserts them clear, and the per-CPU control-register in-guest test checks all three beside SMEP, SMAP, and UMIP. Per-thread FP state is the 512-byte FXSAVE image, and Limine clears other CR4 and EFER bits only from base revision 5 (F130)
- [ ] the kernel builds for `aarch64-unknown-none-softfloat`, the counterpart of the soft-float x86 target, so kernel code never touches FP or SIMD registers and a user thread's FP state needs saving only when the CPU switches away from it (§11.6; Limine enters with `CPACR_EL1` zero, so the first SIMD instruction would trap anyway); user code builds for `aarch64-unknown-none`; `rust-toolchain.toml` and the CI `targets:` inputs gain both
- [ ] exception level: Limine chooses it, and a registered marker records whether entry was at EL1 or at EL2 with VHE (`HCR_EL2.{E2H,TGE}` set). The kernel never changes exception level itself. KVM without nested virtualization, and TCG and HVF without `virtualization=on`, enter at EL1; HVF gives a guest EL2 from QEMU 11.1 on an M3 or later, which Phase 21's HVF record uses. At EL2 the same kernel runs as a VHE host (§11.3, §11.4). Only an EL2 entry keeps the Phase 21 hypervisor possible on that machine; at EL1 the VM layer reports that EL2 is unavailable
- [ ] MMU enable: 4 KiB granule, 48-bit VA, TTBR0 for user and TTBR1 for kernel, which maps onto the existing address map split
- [ ] MAIR and the memory attribute policy: Normal write-back for RAM, Device-nGnRE for MMIO. aarch64 has no MTRRs to override a cacheable alias, so the aarch64 physmap maps only the RAM entries of the Limine memory map and never the holes where the GIC, PL011, virtio-mmio, and PCIe windows sit; it splits to 4 KiB where a 2 MiB block would reach into one. MMIO is reached only through `ioremap` as Device. A framebuffer inside a PCIe window the device tree names is mapped Normal non-cacheable. Any other framebuffer, such as QEMU's `ramfb` in guest RAM, is mapped Normal write-back, as §16.1 maps virtio-gpu buffers, since the host reads it through a cacheable mapping and a non-cacheable alias can go stale under KVM or HVF. There is no equivalent of the in-place x86 UC patch, which §11.2 deletes on x86_64 as well
- [ ] the `_start` order table in DESIGN §3.3 gains an aarch64 column, after DOC2 (§10.3) has rewritten it in live order
- [ ] the initrd, built by the same `mkinitrd` from the aarch64 user programs (§11.6), loaded as a Limine module, as §10.5 does on x86_64
- [ ] early serial on PL011; the same `serial online` first line
- [ ] exceptions before the full exception setup report themselves, on both architectures (moved from §20.1): right after the Limine handshake, x86_64 loads an early IDT with no IST and aarch64 points `VBAR_EL1` (`VBAR_EL2` at EL2) at an early vector table; each early handler prints the vector or exception class, the error code or `ESR`, the faulting PC, and CR2 or `FAR` on raw serial, then halts. A test build that faults before `arch::idt::init` (the aarch64 exception setup of §11.3) shows the harness the early handler's line, on each architecture (F136)
- [ ] PL011 receive, polled on the RX-FIFO-empty flag as §0.4 polls COM1's data-ready bit, feeding the §5.3 input multiplexer

### 11.2 Memory
- [ ] the page table format behind the §10.3 trait: descriptor bits, access permissions, `UXN` and `PXN`, the access flag, shareability
- [ ] one physmap policy on both architectures (AGENTS.md rule 10; DESIGN §4.1): the portable physmap builder maps the RAM-typed ranges of the boot memory map (usable, bootloader-reclaimable, executable and modules, ACPI reclaimable, and ACPI NVS) at their physmap alias, with the largest page each range's alignment allows (1 GiB where the CPU has it, `pdpe1gb` or an aarch64 level-1 block), and nothing else, and x86_64 moves onto it. Device MMIO, the LAPIC, I/O APIC, and HPET included, is reached only through `ioremap` with PCD + PWT (Device-nGnRE on aarch64); a firmware table outside those ranges gets a write-back 4 KiB leaf at its alias when it is first read; and each framebuffer is mapped by its own range with the memory type its location needs, so a GOP framebuffer in a 64-bit BAR above all RAM is drawn through a mapped address, and `Fb::new` refuses one it cannot reach, after which the console falls back to serial with a registered marker (F020). No physmap leaf maps device memory, so no RAM frame has a UC alias (F104), and nothing bounds the RAM the buddy takes: `PHYSMAP_CAP`, `physmap_extent`, `patch_physmap_uc`, `ensure_physmap_wb`, and `acpi_init::map_gap`'s UC leaves are deleted, and `make test-e2e-highmem` expects the `pmm:` count of its 9 GiB guest to include the RAM above 8 GiB. Host tests build the physmap for a memory map with a multi-terabyte MMIO descriptor, for RAM that shares a 2 MiB block with MMIO, and for framebuffers at `0x40_0000_0000` and across a 2 MiB boundary, and map no MMIO page; an in-guest test translates every framebuffer page after `paging_init::install`. DESIGN §4.1, §4.3, and §9.2 describe the policy as built in the same commit
- [ ] TLB maintenance with the Inner Shareable broadcast forms, so the §1.2 shootdown hook needs no IPI: `dsb ishst` after the PTE write, then `tlbi vale1is` for a leaf change (`vae1is` when a table page is freed, `aside1is` for one ASID, `vmalle1is` on ASID rollover), then `dsb ish`, and `isb` on the issuing core; the §4.10 KVA-free deferral is satisfied once the `dsb ish` completes. DESIGN §7.9 gains an aarch64 paragraph; the aarch64 invalidation asserts DESIGN §7.9's calling contract in debug builds, although its broadcast waits for no peer
- [ ] break-before-make for any change to a live descriptor's memory type, cacheability, shareability, output address, block size, or Contiguous bit, and for making a non-global (nG) entry global: write an invalid entry, `dsb ishst`, `tlbi ...is`, `dsb ish`, write the new entry, `dsb ishst`, `isb`; permission-only changes, and making a global entry non-global, need only the TLB maintenance above. The portable page-table code is the only writer of live entries and enforces the rule: a store that makes one of those changes to a valid entry is refused unless the entry was made invalid and its TLB maintenance completed first, and host tests cover each refused change. A live block that the change path itself reads or runs from (the physmap holding the page tables, the kernel image) cannot be broken, so such a mapping is built at the granularity it will need rather than split live
- [ ] ASIDs, so a context switch does not flush the TLB
- [ ] cache maintenance for DMA, and for code: whenever a user page becomes executable, `dc cvau` over it (omitted when `CTR_EL0.IDC` is set), `dsb ish`, `ic ivau` (omitted when `CTR_EL0.DIC` is set), `dsb ish`, `isb`, which x86 got for free; ELF load is the first caller, and §12.2 adds the fault path
- [ ] `dma_wmb`, `dma_rmb`, and the §10.3 `dma_mb` are `dmb oshst`, `dmb oshld`, and `dmb osh` on aarch64, as Linux's arm64 DMA barriers are, since a DMA master observes memory in the outer-shareable domain; each helper cites that Arm ARM rule under §11.7's barrier-helper check (F098)
- [ ] the buddy, heap, and KVA allocators unchanged; that is the point of the split

### 11.3 Interrupts and time
- [ ] exception vectors: the sixteen-entry table, synchronous versus IRQ versus FIQ versus SError, and `ESR_EL1` decoded into the same fault kinds the x86 handlers produce; each synchronous vector saves `ESR_EL1` and `FAR_EL1` into the frame before it unmasks DAIF (DESIGN §5.10 rule 9), and §10.6's in-guest test that yields at the top of a user fault's body passes with a data abort, each kill line naming its own fault address
- [ ] GICv2 and GICv3: distributor, redistributors, CPU interface, priorities
- [ ] MSI through the GICv3 ITS (GICv2m on GICv2), so virtio-pci and the Phase 6 MSI-X tests run unchanged; INTx through the device tree's `interrupt-map` as the fallback
- [ ] the generic timer: `CNTVCT` as the cycle counter, the chosen timer's `TVAL` for the tick, the same `next_deadline` interface. The timer and its interrupt come from the device-tree timer node for the entry level: at EL1 the EL1 virtual timer (`CNTV_*`, the `virt` interrupt); at EL2 with VHE the EL2 virtual timer (`CNTV_*` redirected to `CNTHV_*`, the fifth, `hyp-virt` interrupt) when the node has one, and otherwise the EL2 physical timer (`CNTP_*` redirected to `CNTHP_*`, the fourth, `hyp-phys` interrupt), as Linux falls back. QEMU before 9.0, the `virt-8.2` and older machine types of later QEMU, and boards such as the Raspberry Pi 5 omit `hyp-virt`. A registered marker names the timer chosen, and the §11.5 parser's host tests cover four- and five-entry timer nodes
- [ ] a monotonic clock on `CNTVCT` with the same seqlock publication and the same host tests
- [ ] SGIs as the IPI mechanism: reschedule, call-function, panic halt; no shootdown SGI, because §11.2's broadcast TLB maintenance replaces it; each SGI send runs `dsb ishst` before its `ICC_SGI1R_EL1` write and `isb` after it, so the stores it publishes reach the target first (DESIGN §7.6; §11.7 cites the Arm ARM rule)

### 11.4 SMP and per-CPU
- [ ] secondary cores start through PSCI `CPU_ON` on the conduit the device tree's `/psci` `method` names: `hvc` under a hypervisor at EL1 (KVM, HVF, and TCG, each without `virtualization=on`), and `smc` at EL2 and wherever EL3 firmware provides PSCI, bare metal entered at EL1 included. The kernel brings cores up itself rather than through Limine's MP feature, since §19.6 offlining needs it
- [ ] before each `CPU_ON`, the boot CPU cleans to the Point of Coherency (`dc cvac`, then `dsb sy`) everything the entry stub reads, since the core enters at a physical address with the MMU and D-cache off
- [ ] when §11.1 recorded EL2 entry, the stub writes `HCR_EL2` with the boot CPU's value (`E2H`, `TGE`, `RW`, and `SWIO` set) and an `isb`, then `CPTR_EL2`, `CNTHCTL_EL2`, and `HSTR_EL2` with the boot CPU's values and zero to `CNTVOFF_EL2`, and an `isb`, before any `*_EL1` access or the MMU enable; the core arrives at EL2 with its EL2 controls in reset or firmware state, not the VHE state Limine gave the boot CPU (`HCR_EL2.E2H` is clear wherever it is not RES1, as under QEMU's `CPU_ON`)
- [ ] the stub enables the MMU, with the boot CPU's whole `SCTLR_EL1` value (DESIGN §11.4), through a temporary identity map in TTBR0, jumps to the TTBR1 kernel address, then points TTBR0 at the empty user root and invalidates the local TLB, so no identity entry survives
- [ ] each core prints the §11.1 exception-level marker as it comes online; the online mask and the §4.5 rendezvous barrier are shared with x86_64
- [ ] DESIGN §7.3 gains an aarch64 paragraph listing every step of the stub
- [ ] the per-CPU base, chosen once at boot: `TPIDR_EL1` at EL1, `TPIDR_EL2` at EL2, so that Phase 21 guests own `TPIDR_EL1`; the `per_cpu!` accessors unchanged above the seam
- [ ] per-CPU GIC redistributor and timer setup on each core
- [ ] every core releases the debug OS Lock before its first `eret` to EL0, as Linux does: it writes 0 to `OSDLR_EL1` and `OSLAR_EL1`, then an `isb`, since the architecture resets `OSLSR_EL1.OSLK` to 1 and no breakpoint, watchpoint, or software-step exception is generated while it is set; it writes `MDSCR_EL1` with `MDE`, `KDE`, and `SS` clear (DESIGN §7.5, Debug state); an in-guest test reads `OSLSR_EL1.OSLK` as 0 on every core
- [ ] the bring-up failure path: a core that has not arrived by the bring-up timeout stays out of the online mask, and its stack, per-CPU area, and idle TCB are freed only when PSCI `AFFINITY_INFO` reports it `OFF`, and leaked otherwise, since a late core may still start on them; a `CPU_ON` that returns `ALREADY_ON` is logged as a bring-up failure for that core (F032)
- [ ] each core reads its own parameter block, so a core that arrives late never takes the next core's stack (F032)
- [ ] the §4.9 call-function slot is safe to reuse on a weakly ordered CPU: the sender stores `func` and `arg`, then publishes the round with `acked.store(0, Release)`, responders load `acked` with `Acquire` before they read the payload, and the redundant `compiler_fence` goes; the slot gets a loom model with a weakened variant (§10.8) and a `tests/litmus/` test (§11.7) (F109)
- [ ] PSCI `SYSTEM_OFF` and `SYSTEM_RESET` behind the §10.5 `reboot` syscall, through the same conduit

### 11.5 Devices
- [ ] the device tree from Limine's DTB response, parsed in the portable half and host-tested like the ACPI parser: CPUs, GIC and ITS, timer, PL011, PL031, PCIe ECAM with its `interrupt-map`, virtio-mmio, `/psci`, and QEMU's `fw-cfg` node, so the §10.2 command line and the `vmcoreinfo` note use the same fw_cfg interface as on x86_64. RAM comes from the Limine memory map, never from `memory@` nodes, which Limine removes. `/chosen` is ignored as the protocol requires, so the console is the PL011 that `/aliases` names `serial0`, or, where the tree has no `/aliases` (QEMU before 9.1), the first node compatible with `arm,pl011` whose status is okay or absent, in tree order; host tests parse checked-in trees dumped with `-machine dumpdtb=` from QEMU 8.2, with and without `secure=on` (whose disabled secure UART comes first), and from a current QEMU, and each picks the UART at `0x9000000`
- [ ] PCIe through the ECAM the device tree names, so virtio-pci, MSI-X, and the block driver are shared with x86
- [ ] virtio-mmio transport as well, which QEMU's `virt` offers and Firecracker uses by default; each device's register window is its device-tree `reg` range, claimed and mapped as §10.12 claims and maps a BAR (DESIGN §12.3)
- [ ] every virtio queue notification writes that virtqueue's index, as virtio 1.2 sections 4.1.5.2 (PCI) and 4.2.2 (MMIO `QueueNotify`) require. Today virtio-blk's `kick()` writes 0 for every queue, and QEMU's virtio-mmio takes the queue from the written value. An in-guest test at `-smp 4` over `virtio-blk-device,num-queues=4` completes I/O submitted from every CPU (F047)
- [ ] virtio-input for keyboard and pointer, since `virt` has no PS/2 controller; x86 can use it too, and §16.4 builds on it
- [ ] PL031 for the §2.7 wall clock; `RNDR` where the CPU has it, then virtio-rng, behind `/dev/random`
- [ ] the framebuffer console over the Limine framebuffer, which on `virt` needs `-device ramfb`: edk2's virtio-gpu-pci driver is Blt-only and leaves no linear framebuffer after `ExitBootServices`; virtio-gpu gets a native driver in §16.2

Decided: a device tree, not ACPI, on aarch64 until §20.7. edk2 gives the OS either ACPI or a device tree,
and picks ACPI when QEMU generates tables, so the aarch64 command line passes `-machine virt,acpi=off`. A
device tree describes virtio-mmio and INTx routing without an AML interpreter, and it is what boards
ship. ACPI on aarch64 comes in §20.7, on QEMU's `virt` with ACPI and on `sbsa-ref`, reusing the §2.4
parser and the §20.2 interpreter.

### 11.6 User mode
- [ ] `svc` entry and `eret` exit, the user context saved in the same shape the x86 path produces; the return to EL0 masks IRQs (`DAIF.I`) before it loads `ELR_EL1` and `SPSR_EL1` and keeps them masked until `eret`, as §10.6 requires of the x86_64 syscall exit; the `svc` handler unmasks IRQs once the frame is saved, so the syscall body runs with IRQs on, as DESIGN §2.9 rule 3 requires on both architectures
- [ ] `TTBR0` switch on context switch, skipped when the address space is shared; a switch to a kernel thread points TTBR0 at the empty user root, as x86_64 loads the kernel root for an `as_cr3` of 0, until §27.5 decides lazy TLB (DESIGN §7.9)
- [ ] PAN enabled, with `SCTLR_EL1.SPAN` clear so every exception entry to EL1 sets PAN again, and cleared only inside the §10.6 accessors (SMAP's equivalent); PXN on every user page (SMEP's); the §10.6 user-memory tests pass unchanged, except the x86-only `popf` variant, which has no counterpart because EL0 cannot write PAN; that variant and §10.6's `swapgs` `#DB` and `iretq` `#GP` tests are on the `docs/ARCH.md` skip list
- [ ] `TPIDR_EL0` for user TLS; the ELF loader's TLS layout handles variant I on aarch64 and variant II on x86_64
- [ ] the aarch64 syscall convention documented in `docs/SYSCALL.md`: `svc #0`, number in `x8`, arguments in `x0` to `x5`, result in `x0`, errnos as on x86_64. The numbers are the asm-generic table Linux uses on arm64, which musl's aarch64 port and static Linux binaries need. Numbers are necessary but not sufficient: the per-architecture argument order (§10.5) and struct layouts (below, §13.6, §13.8) do the rest
- [ ] `openat` (with `AT_FDCWD`), `dup3`, and `clone` with fork semantics, because asm-generic has no `open`, `dup2`, or `fork`; the user runtime uses these on both architectures, and the legacy calls stay x86_64-only entry points onto the same paths
- [ ] `struct stat` for `fstat` in each architecture's Linux layout: x86_64's 144-byte layout with a 64-bit `st_nlink` before `st_mode`, and the 128-byte asm-generic layout on aarch64; a host test pins sizes and field offsets to values copied from the Linux uapi headers, because the dev host may be a Mac
- [ ] `/sbin/init`, `/bin/sh`, `/bin/tests` from the user crate, built for `aarch64-unknown-none`
- [ ] user FP and SIMD state (V0-V31, FPCR, FPSR) saved on every switch away from a thread whose state is live in the registers, and loaded before that thread's next return to EL0 when the registers hold another thread's state, as Linux arm64 does; a thread's first FP or SIMD instruction traps once (`CPACR_EL1.FPEN`) to set its state up, and no trap switches FP state between threads. Every aarch64 user compiler emits NEON, so trap-driven lazy switching would save nothing, and it is the scheme LazyFP (CVE-2018-3665) retired on x86; this keeps one policy with x86_64's eager FXSAVE (DESIGN §7.5); the kernel itself never touches those registers (§11.1)
- [ ] aarch64 user FP state across `fork` and `execve` follows the §10.6 x86_64 rule: `fork` saves the parent's live V0-V31, FPCR, and FPSR before it copies them to the child, and `execve` starts the new image with V0-V31 zero and FPCR and FPSR 0; `/bin/tests` checks a NEON register and FPCR across both (F069)
- [ ] SVE and SME trap at EL0 and EL1: every CPU clears `CPACR_EL1.ZEN` and `CPACR_EL1.SMEN` (which reach `CPTR_EL2` under VHE), since per-thread FP state holds only V0-V31, FPCR, and FPSR; in-guest under `-cpu max`, an SVE instruction at EL0 gets `SIGILL` (F130)
- [ ] the EL0 and ring-3 environment of DESIGN §11.4, on every CPU at bring-up: `SCTLR_EL1`, `CNTKCTL_EL1` (at EL2 with VHE, the `CNTHCTL_EL2` it names), and `PMUSERENR_EL0` are written whole from values the aarch64 port computes, never read-modify-write, with the event stream at about 10 kHz, as Linux arm64 runs it. The whole write also clears the `SPAN` bit Limine's base revision 6 sets, which the PAN box above needs clear, and the `EL0PCTEN` bit Limine sets in `CNTHCTL_EL2` at EL2 entry. An EL0 `wfi` traps (`nTWI` clear) and the exception handler steps over it, as Linux arm64 does. On x86_64, the §10.6 control-register routine writes CR4 whole, as one value computed from CPUID that holds `arch::cpu::harden`'s bits and §11.1's clears and leaves every bit it does not name clear, `TSD` and `PCE` included, and it clears bit 0 of `MSR_MISC_FEATURES_ENABLES` where `MSR_PLATFORM_INFO` bit 31 enumerates CPUID faulting; the per-CPU control-register test compares each CPU's CR4 with that value. A `/bin/tests` case runs on every CPU of a `-smp 4` guest: at EL0 it reads `CTR_EL0`, `DCZID_EL0` (with `DZP` clear), `CNTVCT_EL0`, and `CNTFRQ_EL0` and runs `dc zva`, `dc cvau`, `ic ivau`, `wfe`, and `wfi` with no signal, and gets `SIGILL` from each of `msr daifset, #2`, a `CNTPCT_EL0` read, a `CNTV_CTL_EL0` write, a `CNTP_CTL_EL0` write, and a `PMCCNTR_EL0` read; at ring 3 on x86_64 it runs `rdtsc` and `cpuid` with no signal, and `rdpmc` gets `SIGSEGV`. The exit gate's nightly `virtualization=on` boot runs the aarch64 case at EL2 entry too

### 11.7 Build, harness, CI
- [ ] `ARCH=` in the Makefile and `--arch` in the harness; one code path, two QEMU command lines; the aarch64 line carries `-machine virt,acpi=off,gic-version=3` (without `gic-version` QEMU picks GICv2 for 8 or fewer CPUs, under TCG and HVF alike), `-device ramfb`, `virtio-keyboard-pci`, `virtio-tablet-pci`, `pvpanic-pci`, and `vmcoreinfo`
- [ ] the DESIGN §8.3 marker contract split into a shared list and one list per architecture; x86-only markers (`gdt ok`, `pic: remapped`, `time: tsc`, `lapic_timer`) get aarch64 counterparts or are listed as x86-only, and the harness picks lists by `--arch`
- [ ] a test verdict leaves the guest in a form that works under TCG, HVF, and KVM: pass is PSCI `SYSTEM_OFF` (QEMU exits 0), and fail triggers `pvpanic-pci`, which QEMU run with `-action panic=pause` reports over QMP as `GUEST_PANICKED`, so the harness takes the §10.7 core before it ends QEMU and fails the run; the harness also requires the serial `begin` and `end` lines, as §1.6 does. Semihosting is not used, because QEMU supports it only under TCG
- [ ] the panic path, backtrace, and symbol table working on aarch64: a frame-pointer walk along the `x29` chain
- [ ] `make test-kernel ARCH=aarch64` and the e2e ladder, including the serial echo through the PL011 and the `sendkey` echo through virtio-keyboard, in the aarch64 CI tier jobs, plus an in-guest tier with `gic-version=2`, as x86_64 has the LAPIC fallback tier
- [ ] the aarch64 CI jobs run under TCG: GitHub-hosted arm64 runners have no `/dev/kvm` (the request was closed as not planned); native-speed aarch64 runs are HVF on the dev host, kept as §10.9 dev-host records
- [ ] the timing tests pass under HVF on the dev host, the same fix as the §10.1 KVM leg
- [ ] the weekly smp-stress job on both architectures
- [ ] a litmus test under `tests/litmus/` for each barrier idiom the aarch64 port uses: the §2.7 seqlock publish and read, the log ring publish, `SpinMutex` release and acquire, the §10.4 wake inbox hand-off, and the virtio descriptor write before the avail index update. herd7 with Arm's `aarch64.cat` shows the bad outcome forbidden, a twin with the barrier removed shows it allowed, and the x86_64 versions run under `x86tso.cat` to record which barriers x86 elides
- [ ] the litmus set also covers the idioms the kernel review found unsound on a weakly ordered CPU: the seqlock writer's release fence after the odd bump, written from the instructions the kernel build emits (an `ldaxr`/`stlxr` loop for `fetch_add` without FEAT_LSE); call-function slot reuse across two rounds; and the store-then-load pairs that need the full `dma_mb` (the avail index store before the `avail_event` or `used.flags` load, and the `used_event` store before the `used.idx` reload) (F016, F098, F109)
- [ ] each barrier helper in the aarch64 port names its litmus test in a doc comment, and `scripts/check_litmus.py` in `make check` fails when a helper names none or names a missing test; an idiom herd7 cannot express (TLB maintenance, the `dsb` before an SGI system-register write) cites the Arm ARM rule instead. `make litmus` runs herd7, pinned and installed from opam on Linux and macOS, on the nightly job beside `make models` (§10.8)
- [ ] `scripts/check_orderings.py` in `make check`: every `Relaxed`, `Acquire`, `Release`, and `AcqRel` ordering outside test code, fences included, has a one-line comment naming the access it pairs with or saying it pairs with none; the existing sites (376 in `src/` outside ktest files at `2daed69`) are annotated in the same PR
- [ ] the §10.1 `make debug` script and the §10.7 forensics on aarch64 `virt`. QEMU's `dump-guest-memory -p` walks guest page tables only on x86, so an aarch64 core is addressed physically. The kernel publishes the physical address of its TTBR1 root table in a `VMCOREINFO` note through QEMU's `vmcoreinfo` device (a fw_cfg file). The hostlib core tool walks those tables to read kernel addresses, since heap and KVA stacks are not linear in the physmap, and writes the virtually addressed core `gdb` opens. The tool reads the aarch64 thread, log-ring, and trace layouts, and converts the flight recorder's raw `CNTVCT` timestamps (§11.3) with the `CNTFRQ_EL0` value the kernel records. `make test-forensics` passes under `--arch aarch64`, so a hung aarch64 guest yields the same symbolized per-CPU PCs, log tail, and trace as x86_64

### 11.8 Stretch: riscv64
- [ ] mostly a test of whether the §10.3 seam was real: SBI, PLIC and CLINT, Sv39 and Sv48 paging, QEMU `virt`
- [ ] a third port that costs a week says the seam is right; one that costs a month says where it is wrong

---

## Phase 12: Fault-driven Memory

**Goal.** Stop pretending memory is eagerly mapped. Make `fork` cheap, `mmap` real, and a page fault a
routine event on both architectures. Slab, background reclaim, and swap used to live here. Slab and
background reclaim are §19.9 and §19.10, because their gates are measurements. Swap is a stretch at the
end of this phase, because nothing before Phase 19 needs it. What stays is reclaim on demand, which the
unified page cache cannot do without.

**Unlocks.** Real program startup costs. Large sparse allocations. File-backed memory, which the dynamic
linker in §14.2 needs. A `fork` cheap enough that Phase 13's pipelines and Phase 14's shell are not
dominated by it.

**Architectures.** Both. The fault decoding differs (the error code on x86_64, `ESR_EL1` on aarch64), and
so do TLB maintenance and the §11.2 I-cache maintenance aarch64 needs whenever a fault, a COW copy, or
`mprotect` installs an executable PTE, and so does access-flag and dirty tracking, which aarch64 does in software where hardware management (FEAT_HAFDBS) is absent (§12.2). Everything from one fault kind upward is shared.

**Exit gate**
- [ ] `fork` of a process with 100 MB resident completes in under 10 ms, measured by the §12.3 ktest in a 512 MiB, 2-CPU guest under KVM on both architectures
- [ ] page faults are counted per fault kind (demand-zero, COW, file, stack), globally and per process; an in-guest test in which one process touches its stack and output buffer, then first-writes N untouched pages of a fresh anonymous mapping, reads its own demand-zero count from the §12.2 per-process counters before and after and finds it risen by exactly N, on both architectures
- [ ] `brk` and anonymous `mmap` regions from §10.5 are demand-faulted: touching one page maps one page, verified by the §12.1 user-anonymous count rising by exactly one per page touched, not by the global free-frame count (F074)
- [ ] `read()` into a COW page of a forked child leaves the parent's page unchanged; `read()` into an untouched `brk` page faults it in rather than returning `EFAULT`
- [ ] the same fault tests pass on x86_64 and aarch64, with the error code and `ESR_EL1` decoded into one fault kind
- [ ] `mmap` of a file, modify, `msync`, and the change is on disk
- [ ] the OOM path kills a chosen process with a logged reason rather than panicking or hanging, never chooses pid 1, reaps a victim that cannot exit, and returns `ENOMEM` when no process is eligible (§12.6)
- [ ] no frame leaks: after 1000 fork, exec, `mmap`, `munmap`, and exit cycles and the §12.6 exhaustion test, the §12.1 user-anonymous and page-table counts return to baseline, and free plus page-cache frames return to the §10.2 quiescent baseline, on both architectures (F074)
- [ ] the `debug_mm` build catches a deliberate use after free and a double free of a frame, and the KASAN build a use after free of a heap block, each as an in-guest test that expects the report; both builds pass `make test-kernel` on both architectures on the nightly job
- [ ] vibefs crash-state enumeration passes every §12.5 workload on the nightly job, over traces recorded from the guest through the page-cache writeback path on both architectures: every flush-epoch subset up to eight writes and every torn superblock recovers to a committed generation at or after the last acknowledged `fsync`
- [ ] tag `phase-12` and release `v0.12.0`

### 12.1 Frame metadata
- [ ] a `Frame` array indexed by physical frame number, allocated at boot from a known-size region
- [ ] metadata per allocation unit (DESIGN §4.6): the unit's head `Frame` holds its count, flags, owner, index, and LRU link, and each tail `Frame` names its head; units are order 0 until §27.4; a const assertion holds `size_of::<Frame>()` to 64 bytes; §19.8 adds a pin count
- [ ] `FrameRef`, one counted reference to a unit: not `Copy`, `try_clone` saturating as Linux's `refcount_t` does and never wrapping, and the last `put` handing the unit back as a `Frames` to be freed once its TLB invalidation completes, never implicitly; a host test saturates a count and finds the unit never freed
- [ ] `Buddy::deallocate` runs in O(`MAX_ORDER`): its double-free check and buddy-merge test read a free bit and order kept per frame in the `Frame` array instead of walking the free lists (`covered_by_free_block`, `in_free_list`), and teardown frees its frames after `PT` drops (§12.3); a host test counts free-list nodes visited per `deallocate` and finds the same count for 128 MiB and 8 GiB pools (F029)
- [ ] every present user PTE holds one count on its unit, whatever the backing, and the page cache holds one of its own (DESIGN §4.6), so teardown, `munmap`, and COW release each leaf by putting the PTE's count: anonymous, COW-shared, and page-cache units alike. A kernel-owned unit (the shared zero page, and later the vDSO pages, packet rings, and dumb buffers) carries a kernel-owned flag and takes no PTE count, and teardown drops only its mapping. Today `teardown_pool` and `unmap_free` free every leaf to the buddy. A host test tears down an address space holding an anonymous, a COW-shared, a page-cache, and a zero-page leaf and checks each unit's final count, the page-cache unit's at the cache's one
- [ ] the global `PT` lock splits into the kernel page-table lock (the kernel half, KVA, `ioremap`, and the physmap) and one page-table spinlock per address space, held in its core (DESIGN §2.11), both at DESIGN §2.1's PT rank. The fault path, `fork`'s COW walk, `munmap`, `mprotect`, teardown, and §12.6's reverse-map unmap take the space's lock; `fork` takes the child's inside the parent's with `lock_nested`, parent first (DESIGN §2.3), and the kernel lock and a space's lock never nest. A reverse-map walker reaches a space as DESIGN §2.11 states and takes no count on it. In-guest at `-smp 2`: while a `kernel_tests` hook holds process A's page-table lock on CPU 0, process B faults a page in on CPU 1 and its fault completes. Today one global `PT` spinlock serializes every page-table change on every CPU
- [ ] an object-based reverse map (DESIGN §4.6), which reclaiming a mapped page-cache page requires (§12.6) and swap reuses: each unit's owner and index; an interval tree of regions per file mapping and a list of regions per anonymous object, their nodes inside each region; each region's page offset in its object, kept across `mremap` and a split; `MAP_SHARED | MAP_ANONYMOUS` backed by an unlinked tmpfs file; one sleeping reverse-map lock per object at DESIGN §2.1's level 3b; a walk read-locks the object and takes one address space's page-table spinlock at a time, never its address-space lock, and takes no count on the space, holding the reverse-map lock for reading while it takes each region's page-table lock; teardown unlinks a region under that lock taken for writing before it frees the region's page tables (DESIGN §2.11); and a region made from another placed after its source in walk order. A host test walks a page mapped by three processes, after a split and after an `mremap`, and finds every PTE
- [ ] accounting by category: kernel heap, page tables, user anonymous, page cache, free; §19.9 adds slab
- [ ] a Verus spike, run once, so that Phase 38's approach meets the real code before the code grows around it: on a branch that is not merged, Verus proves `vibeos-core`'s `Buddy::order_for` and one `alloc`/`free` pair over `Frames`, as this section leaves them, against the §10.8 Kani properties with no bound. `docs/VERIFIED.md` records the Verus release, the commit, the proof time, and what the proof needed from the code (annotations, ghost state, restructuring), or what blocked it. A restructuring the proof needed lands in the allocator in this phase; a blocker reopens §38.1's tool choice before Phase 38 starts. The spike also attempts the concurrent statement for one `FrameRef::try_clone` and `put` pair called from two threads: a unit is released exactly once, and never while a count on it remains. `docs/VERIFIED.md` records whether Verus proved it over the §10.8 seam's atomics as `external_body` functions, needed vstd's atomic types with ghost invariants (such as `atomic_ghost`) and what that changed in the code, or could not state it; a proof that needed no change to the seam's signatures reopens §38.1's split between Verus and TLA+ before Phase 38 starts
- [ ] a `debug_mm` build: freed frames and heap blocks filled with a poison pattern checked on reallocation; freed frames unmapped from the physmap, so a use after free of a frame faults at the access instead of corrupting the next owner; a second free of a frame or heap block panics, naming the earlier free's caller. The buddy keeps its free-list links inside free frames (DESIGN §2.4), so in this build they move into the `Frame` array above. Frames are unmapped and remapped outside the buddy lock, since the page-table lock is never taken under it (DESIGN §2.1). This build maps the physmap at 4 KiB from boot, since splitting a live block is §11.2 break-before-make on the mapping the page-table code reads through
- [ ] in the `debug_mm` build, frames freed during a page walk are unmapped from the physmap only after `PT` drops and the §12.3 invalidation completes, with one batched physmap shootdown per collection, since the physmap unmap takes `PT`, which `addr_space_init::unmap` and `teardown` hold while they free frames today
- [ ] a KASAN build (moved from §18.4): `-Zsanitizer=kernel-address`, which both kernel targets support, with a shadow map over the heap, KVA, and kernel stacks, red zones, and a free quarantine; memory-intrinsic calls are range-checked against the shadow, since `core` and `compiler_builtins` come prebuilt and uninstrumented. The pinned nightly refuses to link a `-Zsanitizer` crate against that prebuilt `core` (an ABI-mismatch error), so this build passes `-Cunsafe-allow-abi-mismatch=sanitizer`; `-Zbuild-std` stays deleted (B2)
- [ ] both builds are named §10.2 variants and run `make test-kernel` on both architectures on the nightly job from this phase on, because frame refcounts, COW (§12.3), and reclaim through the reverse map (§12.6) are where use after free enters
- [ ] the `Frame` array and the KASAN shadow are mapped in kernel-half PML4 slots that exist before the first `AddressSpace`, since `AddressSpace::new` copies PML4 entries 256-511 once and a slot added later is missing from every existing user address space (F101)

### 12.2 Demand paging
- [ ] a real page-fault handler decoding the fault: present, write, user, reserved, instruction fetch (the x86 error code and CR2, `ESR_EL1` and `FAR_EL1` on aarch64), read from the frame the entry stub filled (DESIGN §5.10 rule 9)
- [ ] access-flag and dirty tracking on aarch64, where hardware management (FEAT_HAFDBS) is optional. `TCR_EL1.HA` is set only when `ID_AA64MMFR1_EL1.HAFDBS` is 1 or more; without it, an access-flag fault sets AF on the present PTE and returns. `TCR_EL1.HD` is set only when the field is 2 or more and the CPU is not on Linux's hardware-dirty errata list; without it, a clean page of a writable shared file mapping is installed read-only, and a write fault on it marks the page dirty and makes it writable. The §10.3 page-table seam reports and clears young and dirty the same way on both architectures, so §12.4's writeback and §12.5's aging are shared code; cleaning a page for writeback follows DESIGN §4.3's dirty rule on both paths (each PTE write-protected or its dirty bit cleared, the old dirty bit read by exchange and folded into the page, and the invalidation completed before the page counts as clean), and on the software path the write-protect makes the next store fault again. Linux's `id_aa64mmfr1.hafdbs=` override on the §10.2 command line lowers the field (DESIGN §3.2), and the aarch64 tier jobs run the Phase 12 fault, `msync`, and reclaim tests again with it at 1 and at 0
- [ ] region lookup for the faulting address, then a per-region fault handler
- [ ] anonymous regions faulting in a zero page; a shared read-only zero page until first write
- [ ] file-backed regions faulting in from the page cache. A user access through a file mapping, private or shared, to a page that lies wholly past EOF, or to one whose fill fails with an I/O error, gets `SIGBUS` (`BUS_ADRERR` and the faulting address in `siginfo` once §13.8 builds it), and the same fault inside a §10.6 accessor takes the fixup and returns `EFAULT`, as on Linux. In-guest: a process that maps three pages of a 6 KiB file reads zeros from byte 6144 to 8191, and a child that touches the third page dies of `SIGBUS`, its `user:` line naming the address (DESIGN §5.2); with a `kernel_tests` hook that fails one fill, the faulting process dies of `SIGBUS`
- [ ] ELF `PT_LOAD` segments become file-backed private regions, so the §10.6 fill API no longer copies segment bytes or zero-fills whole bss pages at exec
- [ ] `PT_LOAD` layouts that a page-cache mapping cannot express are loaded by copy: the page holding the end of `p_filesz` is a private copy zeroed past it, and a segment whose `p_offset` and `p_vaddr` differ modulo 4096, or a page two `PT_LOAD`s share, becomes an anonymous private page with the union of the segments' permissions; an in-guest test execs an ELF whose RX and RW `PT_LOAD`s share a page (F031)
- [ ] stack regions growing down on fault, up to a limit
- [ ] a fault that resolves to no region is `SIGSEGV` for user, panic for kernel
- [ ] a fault inside a §10.6 accessor runs the region fault handler before the exception-table fixup, so a copy into an untouched page demand-faults, and a copy into a COW page breaks COW, as a user store would; only an unresolvable fault becomes `EFAULT`
- [ ] the §10.6 fill API resolves each page through the region fault handler with write intent before writing, so exec's stack, TLS, and bss-tail writes never land in a page-cache or COW-shared frame; it holds a `FrameRef` on each page from the resolve until its write completes, so §12.6 reclaim cannot free the frame in between
- [ ] installing an executable user PTE (a file-backed or anonymous fault, a §12.3 COW copy, `mprotect` adding `PROT_EXEC` in §12.4) runs the §11.2 I-cache maintenance first on aarch64, and nothing on x86_64
- [ ] per-process and global fault counters by fault kind, both reported through syscall 500 until §13.9's procfs, where each process's is in `/proc/<pid>/stat`
- [ ] the fault path may block on I/O or allocate with reclaim, but holds no spinlock and keeps interrupts on across either: it looks the region up, reads the PTE and records its value, drops every spinlock before the read or the allocation, then retakes them and looks the region up again. A fault on a file page holds the page busy (DESIGN §2.1 level 3) from the cache lookup to the install, and only while it is busy checks that the page still belongs to the file's mapping and that its index lies below EOF, so §12.4's truncate, which takes each page it removes busy, cannot come between the check and the install. The fault installs only if the region still covers the address with the same backing and, under the page-table spinlock it installs with, the PTE still equals the recorded value: empty for a first touch, the same read-only PTE for a COW break or a write that dirties a clean shared page, the same swap entry for a §12.7 swap-in (Linux's `pte_same` check). Otherwise it releases what it took and retries, and a retry that finds the page past EOF gets `SIGBUS`. A kernel-mode fault outside a §10.6 accessor or the fill API still panics

### 12.3 Copy on write
- [ ] `fork` marks every writable private mapping read-only in both address spaces and increments frame refcounts; it no longer calls the §10.6 fill API, walking the parent's page tables under the chunk rule below; it links each child region into its reverse-map objects, right after its parent region, before it copies any PTE, then copies under the parent's page-table lock and then the child's, and copies no PTE of a shared region or of a private file region with no anonymous page (DESIGN §4.6)
- [ ] a write fault on a private page reuses it in place, making it writable again, only when it is anonymous and its unit's count is 1, which under DESIGN §4.6 means this PTE is its only holder; otherwise it copies, and a file page mapped privately is always copied, since the cache's own count keeps it above 1; §19.8 refines this for pinned pages
- [ ] `fork` copies no PTE of a `MAP_SHARED` region, file or anonymous (DESIGN §4.6): the child faults the shared pages in from their mapping, writable where the region is, with no COW; in-guest, a write through a shared mapping in the child is visible to the parent, for a file mapping and for an anonymous shared one
- [ ] each `AddressSpace` records the set of CPUs that may hold its translations (DESIGN §7.9): a CPU sets its bit in the space it switches to before it loads the root, with a full barrier between that store and the first walk through the new root (x86's CR3 write serializes; `dsb ish` and `isb` on aarch64), and clears its bit in the space it leaves once the new root is loaded, since without PCID the load flushes that space's entries (on aarch64 the broadcast TLBI reaches every CPU whatever the set says); a round's initiator stores the PTE, runs a full barrier, and then reads the set. This happens in `switch_cr3_for` and in the `addr_space_init::load_cr3_u64` and `load_kernel_cr3` calls that `execve` and `finish_exit` make. Today `addr_space_init::shootdown_user` invalidates only on the CPU that runs the call, and only when that CPU has the space loaded
- [ ] every CPU in that set has invalidated and acked a user-mapping change before anything relies on it (DESIGN §2.4): an unmap, a permission reduction, the COW write-protect, a `MAP_FIXED` replacement, a dirty-bit clear, and a §12.6 reverse-map unmap each put the frames and page-table pages they drop in the operation's gather (DESIGN §4.3), invalidate locally, send one ranged round to the CPUs in that set (DESIGN §7.9; broadcast `tlbi ...is` on aarch64, §11.2), and wait for the acks. Only then does the gather release its units, do `mprotect`, `munmap`, `mremap`, and `madvise(MADV_DONTNEED)` return, does `fork` make the child runnable, and does writeback count a page as clean; `unmap_free` and `teardown_pool` stop freeing frames inside the page walk
- [ ] shootdown rounds are ranged and targeted (DESIGN §7.9), moved here from §27.5 because the `fork` gate's write-protect cannot afford one broadcast round per page: a round carries its target (an address space, or the kernel), a start address, a page count or all, and a freed-tables flag; the handler flushes the whole target above 32 pages; a user round goes only to the CPUs in the target's set other than the initiator, a kernel round to every online CPU; `shootdown_va`'s one-VA broadcast is gone, and `fork`'s write-protect sends one round for the whole address space. In-guest at `-smp 4` with per-CPU `kernel_tests` IPI counters: an `munmap` of 64 pages by a process whose thread runs on CPU 1 sends no IPI, and a reverse-map unmap from CPU 0 of a page that process maps, while it runs on CPU 1, sends one IPI, to CPU 1 only
- [ ] one chunk rule for every walk over a user address space's page tables (fork's write-protect and copy, `munmap`, `mprotect`, `mremap`, a `MAP_FIXED` replacement, teardown): the walk holds the space's page-table lock for at most one leaf table (512 PTEs) at a time with IF on between, records its position as a virtual address, rereads each PTE after it retakes the lock, since §12.6's reverse-map unmap can change one in between, and frees no frame it dropped until a shootdown that covers it has completed; in Phase 12 only the space's own thread changes its regions, and from §13.1 the walk holds the address-space lock for writing (fork: the parent's) across its chunks (DESIGN §2.9 rule 2). A host test through the stub `arch` walks a sparse 4 GiB space and finds at most 512 PTEs visited per hold, and a hook that clears a PTE between two chunks finds the walk skips it
- [ ] in-guest at `-smp 2`: a user process on CPU 0 reads one page in a loop while a kernel thread on CPU 1 unmaps that page through the reverse map, allocates the frame again, and fills it with a pattern; the reader faults and never reads the pattern
- [ ] the write-protect step's shootdown cost recorded in DESIGN §8.6 beside the `fork` numbers below
- [ ] in-guest: fork, write in the child, assert the parent's memory is unchanged; the same with the child's write done by a `read()` syscall into the page; each write by the child raises the §12.1 user-anonymous count by exactly one (F074)
- [ ] a ktest that times `fork` of a process with 100 MB resident against the kernel's monotonic clock and fails at 10 ms or more; it skips with a reason unless the guest has at least 512 MiB and the §10.2 command line carries `accel=kvm` or `accel=hvf`, which the harness sets from the accelerator it launched (DESIGN §3.2), so the per-push TCG tiers skip it. The §10.1 KVM leg runs it on x86_64 as its own step with `VIBEOS_MEM=512M` and `VIBEOS_KTEST` naming it, and the harness fails a run in which a test that `VIBEOS_KTEST` names without a glob skips or does not run, so a skip turns the nightly job red; the aarch64 number is taken the same way under HVF on the dev host as a §10.9 record and recorded in DESIGN §8.6
- [ ] the COW break decision (read the unit's count, copy or reuse, swap the PTE, drop the old reference) in the portable half with a loom model (§10.8): parent and child write-faulting the same frame on two CPUs, and a fault racing §12.6 reclaim unmapping the frame through the reverse map; and a reverse-map walker racing exit teardown and `munmap` in DESIGN §2.11's order; a mutant that reads the refcount outside the page-table lock must fail the model, and so must a mutant that frees a region's page-table pages before unlinking the region, and one that frees a table page two regions share after unlinking one of them but before zapping and unlinking the other; so must a mutant in which one PTE takes no count on its unit; the model also runs a round's completion against the gather's release, and a dirty-bit clear against a store through a stale entry, with a mutant that releases a unit before the acks and one that clears a dirty bit without folding it into the page, both of which must fail; and a CPU switching address spaces races a round, with a mutant that clears its bit in the outgoing space before the new root is loaded and one that sets its bit in the incoming space after the load, both of which must fail; and `fork`'s link and copy racing a reclaim walk, with a mutant that links a child region after copying its PTEs and one that places it before its parent in walk order, both of which must fail. The model also covers §12.2's install check and §12.4's truncate: a COW break whose copy is allocated with the page-table lock dropped installs against the recorded read-only PTE, and a fault racing a truncate of its page never leaves that page, or a private copy of it, mapped after the truncate returns. Three more mutants must each fail the model, by livelock or by a wrong install: one that tests the PTE for empty instead of the recorded value, one whose fault checks membership and EOF before it takes the page busy, and one whose truncate walks the reverse map only once. If the model finds a walker that reaches freed tables under that order, or the reverse map needs a per-page walk that cannot hold an object lock (per-PTE chains), the no-count walk is revisited, and a put that reclaim makes always hands its release to a workqueue worker
- [ ] the eager-copy `fork` of the 100 MB process measured before this subsection replaces it, and demand-zero, COW-break, and file-fault latency after, in the same guests as the ktest above, recorded in DESIGN §8.6 per architecture; §19.3's page-fault microbenchmark starts from these numbers

### 12.4 mmap
- [ ] `mmap`, `munmap`, `mprotect`, `mremap`, `msync`, `madvise`; `brk` and anonymous `mmap` from §10.5 become lazy behind the same interface; a path that removes a region (`munmap`, `mremap`, a `MAP_FIXED` replacement, an `mprotect` merge, `execve`, exit) drops its file and page-cache references only after it releases the region table's lock, and `msync` takes its file references, releases that lock, and then writes back (DESIGN §2.1)
- [ ] anonymous private, anonymous shared (an unlinked tmpfs file, as Linux's shmem, so its pages have a mapping and an index; DESIGN §4.6), file private, file shared
- [ ] Linux's heuristic overcommit (`vm/overcommit_memory` 0): `mmap`, `brk`, `mremap` growth, stack growth, and an `mprotect` that makes a private mapping writable charge the pages they add to a commit count when the mapping is private, writable, and not `MAP_NORESERVE`, as Linux's `accountable_mapping` decides, and a shared anonymous mapping charges its size when it is made, as Linux's shmem does; unmapping uncharges them. One request larger than RAM plus swap fails with `ENOMEM`, and any other succeeds and is served by faults, so the §12.6 OOM killer, not `mmap`, answers a shortage. The count is `Committed_AS` in §14.9's `/proc/meminfo`, and §23.4 adds Linux's other two modes. In-guest, in a 1 GiB guest without swap: a 10 GiB `PROT_NONE` private anonymous mapping and a 10 GiB writable `MAP_NORESERVE` one succeed, a 2 GiB private writable one returns `ENOMEM`, and a 512 MiB one succeeds, raises the commit count by 512 MiB, and lowers it again at `munmap`
- [ ] `MAP_FIXED` handling, including replacing existing mappings, with `munmap`, `mprotect`, and `mremap` walking under §12.3's chunk rule
- [ ] region splitting and merging on partial unmap and protect, under the write lock of every reverse-map object the region is linked into, a file mapping's before an anonymous object's (DESIGN §4.6); `munmap` zaps a region's PTEs through the gather before it unlinks the region
- [ ] an `mremap` that moves a region places the destination after the source in each object's walk order, or, where it cannot, holds each object's reverse-map lock for writing across the PTE move (DESIGN §4.6); in-guest at `-smp 2`, an `mremap` on CPU 0 moves a file-backed region to a lower address while a `kernel_tests` hook on CPU 1 unmaps the file's pages through the reverse map, and after both finish no PTE of the moved region maps a page the walk unmapped
- [ ] on x86_64 a non-present user PTE that keeps a frame number, such as a `PROT_NONE` page, stores it inverted, as Linux does against L1TF (CVE-2018-3620), so no non-present PTE names cacheable RAM; a host test decodes each non-present encoding (F133)
- [ ] a VA space allocator for the user half, below the §10.6 `USER_MAP_END`, that places mappings as Linux's default layout does: top-down from an `mmap_base` one stack-size gap below the stack (Linux's `mmap_base()` rule, with the stack's `RLIMIT_STACK` and a gap of at least 128 MiB), honoring a hint address when the range there is free, and bottom-up only under `ADDR_COMPAT_LAYOUT` or an unlimited `RLIMIT_STACK`, as `mmap_is_legacy` decides; host tests place a sequence of mappings and compare the addresses with those a Linux x86_64 and arm64 process gets for the same requests with randomization off (`ADDR_NO_RANDOMIZE`, as `setarch -R` sets)
- [ ] dirty page writeback for shared file mappings, which cleans each page under DESIGN §4.3's dirty rule before it writes it, so no store made through any CPU's stale entry is lost
- [ ] a truncate that shrinks a file follows Linux's `truncate_pagecache` order: it stores the new size; unmaps every page wholly past it from every mapping of the file through §12.1's reverse map, private copies that COW breaks made from those pages included; removes those pages from the page cache, taking each busy and unmapping it first if a fault mapped it meanwhile; unmaps the same range once more, for a private copy a fault made while it held a page busy during the removal; and zeroes, in the cache, the tail of the page that holds the new EOF, as §13.9 zeroes it on disk (F125). A later access to an unmapped page faults and gets §12.2's `SIGBUS`. Until §13.9 adds `truncate` and `ftruncate`, the tests call `Vfs` truncate. In-guest: a process maps the three pages of a 12 KiB file both `MAP_SHARED` and `MAP_PRIVATE`, writes to every page through each, and truncates the file to 6 KiB; through the shared mapping bytes 6144 to 8191 read as zero, a child that touches the third page through either mapping dies of `SIGBUS`, and after the file is extended to 12 KiB again `read` returns zeros from byte 6144. At `-smp 2`, 1,000 rounds: a child reads the third page of a `MAP_SHARED` mapping in a loop, reading a flag in a shared anonymous page before each read, while the parent truncates the file to 6 KiB and then sets the flag; every child dies of `SIGBUS`, and a child whose read succeeds after it saw the flag exits 1, which fails the test
- [ ] `mlock` for pages that must not be evicted

### 12.5 Unified page cache
- [ ] one page cache of mappings (DESIGN §10.6), rather than a block cache and a page cache disagreeing: every cached page belongs to one file mapping (file data, tmpfs, the ELF pages §12.2 maps) or one block-device mapping (filesystem metadata and the device node) at one page index, and a device location is never a key; today's `(dev_id, page offset)` cache becomes each block device's mapping, and one frame pool, one LRU, and one set of writeback threads serve both kinds
- [ ] the buffered `write` path copies into a busy page only through a non-faulting variant of the §10.6 accessor that returns a short count at the first fault; on a short count it releases the page, faults the rest of the source in with no page held, and retries (DESIGN §2.1); in-guest, a `write` at offset 0 of 8 KiB whose source is a `MAP_SHARED` mapping of the same file's first two pages, the second not yet faulted in, returns 8192 and leaves those bytes unchanged
- [ ] one radix tree per mapping, keyed by page index, with 64-way nodes (Linux's XArray shape), allocated before the mapping's RANK_DEVICE lock is taken and freed after it is dropped, and the lock never held across I/O; the mapping holds a counted inode reference and its backend's fill and writeback operations, so the fault path and the writeback threads never take the VFS lock (DESIGN §2.1); a host test inserts and removes pages with the allocator failing at every step and finds the tree unchanged after each failure
- [ ] vibefs writes file data back through its mappings (DESIGN §10.6, VIBEFS.md §10): `write` dirties file-mapping pages and allocates no block, and a commit writes each dirty page to a fresh block, submitting the page's own frame, so the page keeps its mapping, index, and frame while the file's block map moves; a block vibefs allocates loses any device-mapping page for its LBA before its first write, with that page's dirty state discarded and any in-flight writeback of it completed first. Host tests: a file page written and committed twice keeps its mapping entry and frame while its block changes; a device-mapping page of a freed block, once dirty and once in writeback, is gone before its LBA is written again as file data, the data write waits for the old writeback, and the disk ends with the file data
- [ ] page-cache pages, today's block-cache slots first, carry a FILLING state (key visible, readers wait on a per-slot wait queue that replaces the yield spin) and a WRITEBACK state (readable, no second write to that LBA until the first completes); a dirty victim keeps its old key until its writeback completes, and §10.11's `flush` wait for in-flight writeback becomes a wait on that device's WRITEBACK slots; a host test interleaves `plan`, `install`, and writeback from two callers and finds no duplicate key, no stale read, and no device flush ahead of an in-flight write (F015)
- [ ] each block `Queue` carries its device's transfer limits (`max_bytes` and `max_discard_sectors`; `BOUNCE` and `MAX_DISCARD` for vda), `merge_into` refuses a merge past either, and `submit` returns `Inval` for a single request past them; a host test queues three adjacent 4 KiB writes against an 8 KiB limit and gets two requests that both succeed (F119)
- [ ] async block submission owns what it borrows: `block_init::submit` and `virtio_blk_init::submit` take an owned buffer (a page-cache frame reference held until completion) and a pinned, reference-counted `IoWaiter` instead of `ptr: usize` and `&IoWaiter`, and `Seg.ptr` and `Request.waiters` are private, so a submitter that returns early cannot leave a completion aimed at its stack frame; the completer drops its `IoWaiter` and buffer references after it leaves `SCHED` (DESIGN §10.1), and a last drop in the bottom half defers its release (DESIGN §2.11 rule 6); a `compile_fail` doctest in `block.rs` shows a borrowed stack buffer refused (F042)
- [ ] one bottom-half thread per threaded vector, pinned to the CPU the vector is routed to and moved by §6.3's `set_affinity`, replacing the one `irqth` thread on the last online CPU (DESIGN §5.4); a bottom half takes no sleeping-tier lock, waits for no other device's I/O, and allocates without direct reclaim, in DESIGN §4.4's atomic class; a block completion allocates nothing, since what it needs was allocated at submission (the owned-submission box above, F042). In-guest at `-smp 4`: virtio-blk with 4 queues completes I/O submitted from every CPU, each queue's bottom half running on its vector's CPU by per-thread CPU counters; and one virtio-blk queue's bottom half held at a `kernel_tests` hook delays neither the other queues' bottom halves nor virtio-rng's
- [ ] block requests time out (DESIGN §10.3): each carries a deadline, 30 s by default and settable per device, after which the driver's timeout handler gets it back before its buffer is touched; virtio-blk resets the device (status 0, read back as 0), fails or resubmits every request it held, and brings the queues back, and a device the reset does not recover goes `Failed`; `IoWaiter::wait` parks on the request's deadline instead of `FAR_DEADLINE`. In-guest: a `kernel_tests` hook discards one used-ring entry, and that request completes with `Io` after the deadline, which the test sets to 1 s, while the next request on the same queue succeeds; the hook records that the timed-out request's buffer was released only after the device status read back 0
- [ ] writeback threads with per-inode ordering and a dirty limit that throttles writers before direct reclaim finds only dirty pages; they are no-reclaim, progress-class threads, whose allocations may use all of §12.6's reserve (DESIGN §4.4)
- [ ] one LRU with second-chance aging, which §12.6 reclaims from; §19.10 splits it into an active and an inactive list
- [ ] readahead driven by detected access patterns
- [ ] `tmpfs` pages living in each file's mapping with no device behind them (DESIGN §10.6), replacing the §8.4 version that sits on the block cache plus a fixed ramdisk, so a full `tmpfs` is reclaimable rather than pinned
- [ ] a volatile-cache block device: an NBD server in hostlib, advertising flush so QEMU forwards every guest flush, serves the guest's virtio-blk disk with `write-cache=on`, so every flush is the guest's own, and records every write and flush the page cache and writeback threads issue. Killing QEMU, as the §8.5 test does, loses no write the host already received, so only this device can catch a missing flush. It lands before the writeback threads replace today's write path
- [ ] a crash-state enumerator on the host rebuilds the disk from a trace: after each flush epoch, the durable prefix plus every subset of that epoch's writes up to eight, a seeded sample of larger subsets, and every superblock write torn at a 512-byte sector boundary, so each state costs a host mount, not a boot; the host tests of the portable volume run the same enumeration beside `CrashDisk`'s suffix drop
- [ ] the oracle checks content, not only `fsck`: the mounted tree equals the tree of a committed generation, and that generation is at or after the last `fsync` or `sync` that returned before the crash point
- [ ] vibefs `commit` keeps the new generation and roots in locals until the super flush succeeds and writes the slot opposite the super this mount last mounted or committed; an error before the super write releases the blocks the transaction allocated, and an error in the super write or the flush after it keeps them until a later commit's super flush succeeds, since that super may be on disk (VIBEFS.md §10). A host test with a `Disk` that fails one write or flush at each commit step, once before the write reaches the media and once after, retries the commit, tears the retry's super write, and mounts a committed generation whose blocks are intact every time (F050)
- [ ] vibefs snapshot delete, which the workloads below use and v1 lacks (`Vol::snapshot` only creates): it walks the snapshot's inode and directory trees, drops one reference per reachable block, and frees the snapshot slot; a host test overwrites blocks under a snapshot, deletes it, and finds free space back at its value before the snapshot (F067)
- [ ] workloads: `crash_at_each_write_is_consistent`'s script; a temporary file written, `fsync`ed, and `rename`d over the original, as editors and compilers save; `fsync` of a file and then of its directory; a snapshot created and deleted mid-workload. In this phase they run as in-guest tests through `Vfs`, whose `rename` exists and which gains a per-inode `fsync` with the writeback threads above, since the `fsync`, `renameat`, and `sync` syscalls are §13.9's
- [ ] FAT enumerated against its weaker oracle: `fsck.fat -n` finds no directory entry pointing at a free cluster and no cross-linked chain

### 12.6 Reclaim and OOM
- [ ] direct reclaim when an allocation fails, under DESIGN §4.4's reclaim rules: it drops clean page-cache pages that nothing maps, and unmaps mapped ones through §12.1's reverse map under each address space's page-table spinlock and try-locks of the page and of its reverse-map lock, exchanging each PTE to empty and folding its dirty bit into the page; once the invalidation completes it drops the pages it found clean and leaves the dirty ones, unmapped, to the writeback threads; it skips any page it cannot take at once; it takes a sleeping lock only by try-lock, never the address-space lock, and writes no page, and when that frees too little it wakes the §12.5 writeback threads and waits, with a deadline, only for writes already submitted to a device; §19.10 adds watermarks and a background thread
- [ ] in-guest at `-smp 4` in the KASAN build: a `kernel_tests` hook runs reclaim's reverse-map walk over a `MAP_SHARED` file's pages while 1,000 forked children map the file and exit; no KASAN report, and the §12.1 user-anonymous and page-table counts return to baseline
- [ ] one reserve, R frames of the buddy's free count, sized at boot as Linux sizes `min_free_kbytes` (DESIGN §4.4): a general allocation stops at R, an atomic-class one (IF=0, a spinlock held, or an RCU read-side section; a softirq-equivalent item; a threaded bottom half) at R/2, and a progress-class one (the §12.5 writeback threads, §12.7's swap-out thread, a thread while it runs direct reclaim, §19.10's background reclaim thread) may use all of it; `meminfo` shows R and each class's low-water mark. A fault or `mmap` allocates the page-table pages it may need before it takes the page-table spinlock, with reclaim allowed, and frees those it did not use. In-guest: a `kernel_tests` softirq-equivalent item allocates frames and holds them until an allocation fails, while a kernel thread writes a file on `vda` through `Vfs` past the dirty limit and calls its per-inode `fsync` (§12.5); the atomic class's low-water mark is at least R/2, the `fsync` returns, and the progress class's low-water mark is above zero
- [ ] the general heap becomes a two-level segregated-fit allocator (TLSF) in `vibeos-core` (DESIGN §4.4): constant-time `alloc` and `free` with boundary-tag coalescing, alignment up to a page, growth by appending a free block, and a moving `realloc` that allocates under HEAP, copies with HEAP dropped, and frees under HEAP; host tests, a Kani proof on a small arena (blocks disjoint, neighbours coalesced, free count exact over every sequence of up to six calls), and a host test that fragments the heap into 100,000 free blocks and finds each `alloc` and `free` visiting a constant number of free-list heads
- [ ] the nightly `-icount shift=0` run of §10.3's `irqoff` build fails on any logged stretch, on both architectures (the aarch64 exception vectors stamp the tracer as §10.6's stubs do). It lands after the TLSF box above, §12.1's O(`MAX_ORDER`) `Buddy::deallocate` (F029), §12.3's chunk rule, and a box in this section for every other site the §10.3 runs have logged; DESIGN §2.7's I31 row then reads enforced
- [ ] the heap region sized at boot from installed memory, up to the 16 TiB below the KVA region, in place of DESIGN §4.1's fixed 64 MiB, so a heap allocation fails only when frames do and the OOM killer never runs while frames are free (DESIGN §4.4); `meminfo` reports the region's size beside the heap's use; in-guest, in a 1 GiB guest, a `kernel_tests` hook grows the heap past 256 MiB of live 4 KiB objects, and no process is killed
- [ ] direct reclaim runs only from an allocation that may sleep: the allocator reads at entry, before it takes the heap lock, whether interrupts are on, this CPU's §4.7 `HELD` mask is empty, and the calling thread is not a no-reclaim thread (the §12.5 writeback threads, §12.7's swap-out thread, threaded interrupt bottom halves, a workqueue worker while it runs a softirq-equivalent item, and a thread already in reclaim); every other allocation draws on the reserve, as deep as its class allows, and then fails instead of reclaiming (DESIGN §4.4)
- [ ] an OOM killer over a scope, the whole machine here and a cgroup from §21.5: it chooses among the scope's user processes, never pid 1 (§10.5) or a kernel thread, the one with the highest score, Linux's `oom_badness` base: resident pages plus page-table pages, plus swap entries once §12.7 lands; it logs the score table, one column per part, before it acts; §19.10 adds Linux's `oom_score_adj`
- [ ] one victim per scope at a time: while a victim's memory is still to be released, the killer chooses no other. When the allocating thread's own process is exiting or has a fatal signal pending, that process becomes the victim without a scan, and its allocation fails with `ENOMEM` instead of waiting for itself, as Linux's `task_will_free_mem` check does; a chosen process that is already exiting gets no signal. Every other victim gets `SIGKILL`, and from §13.1 so does every process that shares its address space (`CLONE_VM` without `CLONE_THREAD`, as `vfork` makes), as Linux's `__oom_kill_process` does; a victim whose address space pid 1 shares is killed but never reaped. The victim's threads draw on the reserve down to R/2 while they exit (DESIGN §4.4)
- [ ] an OOM reaper kernel thread, in DESIGN §4.4's progress class, unmaps each victim's private mappings, the anonymous pages of private file mappings and locked pages included, through the §12.3 invalidation, without waiting for the victim to exit. It takes the lock on the victim's region table only by try-lock, up to ten tries 100 ms apart, as Linux's `oom_reaper` does. Once it has unmapped any of a victim's memory, no fault in that address space is served with a new page: a user fault returns to the pending `SIGKILL`, and a fault inside a §10.6 accessor takes the fixup and returns `EFAULT`, so a victim inside `write` never copies a fresh zero page into a file, as Linux's `MMF_UNSTABLE` ensures. The victim's memory counts as released when the reaper finishes, when the victim exits, or when the ten tries fail, which is the deadline; `meminfo` counts OOM kills and the victims that reached the deadline unreaped. The in-guest tests below and §19.10's pressure measurements read that count, and a victim that reaches the deadline unreaped while it could exit means the ten-try bound, or the short hold of the victim's region-table lock that it relies on, is wrong, and reopens the deadline
- [ ] the allocation that ran the killer waits, killably, until its victim's memory is released, then runs direct reclaim and the allocation once more before it returns `ENOMEM`. With no eligible process in the scope it returns `ENOMEM` at once and logs one rate-limited `oom: no eligible victim` line; nothing on this path panics (DESIGN §4.4). A user page fault whose allocation still fails after this returns to user mode and is retried, as Linux's `pagefault_out_of_memory` retries, after a 100 ms sleep when no process was eligible, so the faulting process dies only if the killer chooses it, and a fault in pid 1 waits; a fault inside a §10.6 accessor takes the fixup and returns `EFAULT`
- [ ] allocation failure follows DESIGN §4.4: a fallible allocation that still fails after direct reclaim, the OOM killer, and the wait for its victim returns `ENOMEM` to its caller, which unwinds what it built; only an infallible allocation (boot time, or bounded by a kernel invariant) panics, and it prints the full memory state from §12.1's accounting
- [ ] a pending `SIGKILL` ends a process that only touches memory: §10.6's check on every return to ring 3 also runs on every return to EL0; the page-fault handler checks for it before it allocates; and a kill aimed at a thread running on another CPU sends that CPU the §4.9 reschedule IPI (an SGI on aarch64); §13.8 extends this path to handled signals
- [ ] in-guest: allocate to exhaustion and assert the system survives, with the expected process killed; the victim makes no syscall after its first `mmap` and only touches memory
- [ ] in-guest, each with its score table logged: a victim held in an uninterruptible wait at a `kernel_tests` hook is reaped, and the allocation that chose it succeeds within the reaper's deadline with no second process killed; with pid 1's resident size made the largest by a `kernel_tests` hook, another process is chosen and the kernel stays up; a victim held inside `write` from a buffer that the reaper then unmaps resumes and gets a short count or `EFAULT`, and the file holds only the buffer's pattern; a process held at a `kernel_tests` hook in its exit teardown is chosen, gets no second signal, and is reaped, and no other process is killed; with a `kernel_tests` hook that makes every process ineligible, an allocation returns `ENOMEM` with one `oom: no eligible victim` line, and a user fault waits and then completes once the hook is released; a lone process that touches memory until none is left is chosen itself
- [ ] in-guest: while dirty pages of a FAT volume on `vda` fill most of the page cache, a kernel thread that holds that volume's lock allocates until direct reclaim runs; every allocation returns a frame or `ENOMEM` and the thread finishes, so reclaim never waits on a lock its caller holds; a `kernel_tests` counter shows that direct reclaim wrote no page
- [ ] in-guest at `-smp 2` on both architectures: a process on CPU 1 stores an increasing counter into a `MAP_SHARED` page of a file on `vda` in a loop while a `kernel_tests` hook on CPU 0 repeatedly cleans that page for writeback and unmaps it through the reverse map for reclaim; after the writer's last store and an `msync`, the file holds the writer's last value (DESIGN §2.4, §4.3)

### 12.7 Stretch: swap
Nothing before Phase 19 needs swap; a self-hosting build is given RAM, not swap. Not gating.

- [ ] allocate well past physical memory with swap enabled and the workload completes
- [ ] a swap device or file with a slot allocator
- [ ] page-out in a swap-out thread, a no-reclaim thread, never in direct reclaim (DESIGN §4.4): pick a victim, write it, replace the PTE with a swap entry, free the frame
- [ ] on x86_64 a swap-entry PTE keeps its present bit clear and stores the slot offset inverted in its address bits, as Linux does, so no non-present PTE names cacheable RAM (L1TF, CVE-2018-3620) (F133)
- [ ] page-in on fault from the swap entry
- [ ] §12.1's reverse map finds every PTE mapping an anonymous page being swapped, through its anonymous object
- [ ] readahead on swap-in, since thrashing one page at a time is unusable
- [ ] a swap cache to avoid duplicate I/O for a shared page
- [ ] `swapon` / `swapoff`, with `swapoff` faulting everything back in

---

## Phase 13: Threads, IPC, Signals, and the POSIX Surface

**Goal.** Processes with more than one thread, that talk to each other, respond to events, and present
enough of a POSIX surface that real software can be ported without patching every call site.

**Unlocks.** Shell pipelines. Job control. `pthreads`. Anything ported from Unix. Unmodified static Linux binaries, checked against Linux itself (§13.11).

**Architectures.** Both. Signal frames, `sigreturn`, the TLS register, `clone`'s argument order, and the
user-visible structs Linux defines per architecture (`stat`, `epoll_event`, `sigaction`, `ucontext`) are
per architecture, each pinned by a size-and-offset host test; the rest is shared. Every call lands under
its asm-generic name, which both architectures have; the legacy x86_64 names are entry points onto it. §13.11's Linux oracle for each architecture is a GitHub-hosted Linux runner of that architecture, which runs a static binary natively without KVM.

**Exit gate**
- [ ] `ls | grep foo | wc -l`, typed into the §13.7 `/bin/sh` with the §10.5 utilities, works with correct exit statuses and no deadlock on a full pipe
- [ ] ctrl+C kills the foreground job and leaves the shell alive; ctrl+Z stops it and `fg` resumes it
- [ ] ctrl+C kills, within 1 s, a foreground job that makes no syscalls (a user-crate program spinning in a loop), on both architectures
- [ ] a user signal handler runs on a proper user stack and returns correctly through `sigreturn`, a crafted frame cannot leave user mode or crash the kernel, and a process killed by `SIGSEGV` leaves a core that host `gdb` backtraces to the faulting function (§13.8)
- [ ] `ppoll` on the read ends of 100 pipes, one of which another thread writes after 100 ms, returns 1 with `revents` set on that descriptor only, and `getrusage(RUSAGE_THREAD)` shows `ru_nvcsw` risen by exactly one across the call (F150)
- [ ] four processes sharing one `memfd_create` mapping each take the user crate's process-shared futex mutex in it (§13.1) and increment a counter in the mapping 10,000 times; the final count is 40,000, and a process blocked on the mutex for 1 s while another holds it accrues under 10 ms of CPU time across the lock call, by `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`
- [ ] a Unix domain socket carries a passed file descriptor between processes
- [ ] sixteen user threads in one process contend a futex mutex from the user crate: the count is right, `exit_group` tears all of them down while some are blocked in `read`, and the §12.1 user-anonymous and page-table counts return to baseline (F074)
- [ ] `ps` and `/proc/<pid>/*` come from the process table; syscall 500 is gone
- [ ] unmodified static `busybox` and `toybox` from the §13.11 corpus pass the differential runner on both architectures, busybox's own test suite passes minus its checked-in expected-failure list, and the pipeline and job-control lines above pass again with that `busybox` as `/bin/sh` and the utilities
- [ ] on the nightly job, on both architectures: 10,000 generated syscall sequences agree with Linux on every return value, errno, and resulting file tree, on vibefs and on tmpfs, and the §13.11 LTP subset passes except its checked-in expected-failure list; every accepted difference is in `docs/LINUX.md`
- [ ] the in-guest ladder in the §13.12 lock-dependency build reports no cycle, no IRQ-safety inversion, and no sleep under a spinlock on both architectures, and it does report the planted same-rank AB-BA test
- [ ] the §13.13 fuzzer runs one hour per architecture on the nightly job, in a 2-CPU, 512 MiB guest (KVM on x86_64, TCG on aarch64), with no kernel panic, hang, KASAN report, lock-dependency report, or leaked frame, and every crash it has found is a checked-in replay
- [ ] tag `phase-13` and release `v0.13.0`

### 13.1 Threads
- [ ] `clone` with `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID`; a plain fork is `clone(SIGCHLD)` on both architectures (§11.6), and x86_64 keeps the `fork` number as an entry point onto it. `CLONE_SYSVSEM` is accepted as a no-op and `CLONE_DETACHED` ignored, since musl's `pthread_create` passes both; `CLONE_VM | CLONE_VFORK` gets vfork semantics, since musl's `posix_spawn`, `system`, and `popen` use it
- [ ] `exit` ends a thread and `exit_group` ends the process; the last thread out drops the last `users` reference, which runs the teardown (DESIGN §2.11)
- [ ] a process holds the list of its threads instead of the one `tid` in `Proc` today, and `getpid` returns the thread-group id in every thread (F077)
- [ ] each `clone` flag shares the object Linux shares, and without the flag the child gets a copy: `CLONE_VM` the address space, `CLONE_FS` the working directory, root, and umask, `CLONE_FILES` the descriptor table, and `CLONE_SIGHAND` the handler table; in-guest, a `chdir` in a `CLONE_FS` thread changes its sibling's relative lookups, and one in a `fork` child does not
- [ ] in-guest, one thread calls `munmap` and then `exit_group` while a sibling is inside a `read()` copy, and the copy completes or fails with `EFAULT` with no kernel fault; §10.6 made the address space a counted object (F019)
- [ ] a per-address-space reader-writer lock guards the region table: `mmap`, `munmap`, and `mprotect` take it for writing and the fault path for reading. A page-table entry changes only under the space's page-table spinlock (§12.1), so reclaim never needs this lock (DESIGN §2.1, §4.4). It sits in DESIGN §2.1's sleeping tier after the filesystem namespace and inode locks and before page waits, so code holding it takes no namespace or inode lock (`mmap` of a file takes its page-cache reference before it takes this lock, and a path that removes a region, or `msync`, releases it before it drops the region's references or writes back), while the fault path may still fill a file page through the filesystem's block-mapping locks. An in-guest test runs three threads of one process across two CPUs: one whose `write` to a vibefs file faults on an untouched user buffer page, one that `mmap`s the same file in a loop, and one that calls `msync(MS_SYNC)` on a shared mapping of that file in a loop; all three finish. Today the region table is a fixed array with no lock of its own, reached only under the global `PT` spinlock
- [ ] `execve` in a multithreaded process ends the other threads before it releases the old address space, as Linux's `de_thread` does; in-guest, one thread of a four-thread process calls `execve` while the other three loop in `read()`, and the kernel stays up
- [ ] a narrowed or removed translation is gone on every CPU when the call returns (DESIGN §2.4), in-guest at `-smp 2` on both architectures: a thread on CPU 1 stores to a page in a loop and records any store that succeeded after it saw a flag that a sibling on CPU 0 sets once `mprotect(PROT_READ)` of that page returns, and it records none; and while a thread on CPU 1 stores an increasing counter to a private page, a sibling on CPU 0 calls `fork`, and the child reads the page twice, 10 ms apart, and finds the same value both times
- [ ] the `debug_mm` build asserts in `switch_cr3_for` that the root it loads belongs to a live `AddressSpace`, so a stale `as_cr3` is caught once threads share an address space; §10.3's teardown box zeroes the exiting thread's `as_cr3` (F106)
- [ ] TLS through `clone`'s `tls` argument (the fifth argument on x86_64, the fourth on aarch64): `FS_BASE` on x86_64 and `TPIDR_EL0` on aarch64, plus `arch_prctl(ARCH_SET_FS)` for musl on x86_64; `arch_prctl(ARCH_SET_FS)` and `clone` refuse a base at or above `USER_MAP_END` with `EPERM`, as Linux refuses one at or above `TASK_SIZE_MAX`, so `wrmsr IA32_FS_BASE` never takes a non-canonical value
- [ ] the user TLS base is per-thread state: `on_switch` saves `FS_BASE` (`TPIDR_EL0` on aarch64) for an outgoing user thread and loads it for an incoming one, `fork` and a `clone` without `CLONE_SETTLS` copy the caller's saved value rather than the live register, `execve` sets the new image's value, and `force_kernel()`'s selector load no longer leaves the next user thread at base 0; in-guest on both architectures, a `PT_TLS` process forks a child that exits, then reads back a TLS variable (F022)
- [ ] `set_tid_address` and the clear-child-tid wake, which is what `pthread_join` stands on
- [ ] a per-thread kernel stack, signal mask, and pending set; process-directed signals delivered to one eligible thread
- [ ] `gettid`, `tgkill`
- [ ] `exit_group` while other threads of the group are blocked in a syscall: each blocked thread is woken, does not return to user mode, and exits, and the last one out releases the address space; in-guest, with threads blocked in `read` and in a §13.5 futex wait
- [ ] an open file description shared through threads, `fork`, and `dup` loses no update: each `read`, `readv`, `write`, `writev`, `lseek`, and `getdents64` on a regular file or directory holds the description's position lock (DESIGN §2.1 level 1) across its I/O and its offset update, the file's size changes only under the inode lock, and a pipe, socket, TTY, or character-device description takes no position lock, over §10.4's open-file table, which already keeps `refs` out of every write-back; in-guest, four threads each write 10,000 fixed-size records through one descriptor, and the file holds 40,000 intact records (F055)
- [ ] `spawn_user` gives a fork child or cloned thread `CpuAffinity::Any`, so §4.8's placement puts it on an online CPU instead of pinning it to its creator's CPU, which today keeps all user work on CPU 0. §13.10's affinity mask later narrows the choice. The gate's sixteen-thread test reads `getcpu` in each thread and finds at least two CPUs
- [ ] the user crate gains `thread::spawn`, `join`, and a futex-backed mutex, private by default, with a process-shared mode that uses non-private futex operations (§13.5's shared keys); the gate's two counter tests use it
- [ ] in-guest: the counter test from the gate; create and join a thousand threads, no more than 64 alive at once, within the Phase 10 thread limit; `exit_group` from a non-main thread

### 13.2 Pipes
- [ ] a bounded ring buffer with blocking read and write and correct partial-write semantics
- [ ] read end closed produces `SIGPIPE` and `EPIPE`; write end closed produces EOF
- [ ] `pipe2` with `O_CLOEXEC` and `O_NONBLOCK`, and `pipe` as its x86_64 entry point
- [ ] named pipes through the VFS, created with §13.9's `mknodat`
- [ ] a pipe write of at most `PIPE_BUF` (4096) bytes is never interleaved with another writer's; in-guest, several writers of 4096-byte records, and the reader checks every record whole
- [ ] in-guest: fill the pipe, assert the writer blocks, drain, assert it proceeds
- [ ] a pipe's lock is dropped before its reader or writer sleeps (DESIGN §2.1): in-guest, with one thread blocked in `read` on a FIFO opened `O_RDWR`, another thread's `write` through the same descriptor completes and wakes it, and a second descriptor of that FIFO opened with `O_NONBLOCK` gets `EAGAIN` from `read` while the first thread is blocked

### 13.3 Unix domain sockets
- [ ] `socket`, `socketpair`, `bind`, `listen`, `accept`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `shutdown`, `getsockopt`, `setsockopt` for `AF_UNIX`; `send` and `recv` are runtime wrappers over `sendto` and `recvfrom`, since neither syscall table has them
- [ ] stream, datagram, and seqpacket types
- [ ] filesystem-bound and abstract namespaces
- [ ] `SCM_RIGHTS` file descriptor passing and `SCM_CREDENTIALS` as `sendmsg`/`recvmsg` control messages, with `setsockopt(SO_PASSCRED)` on the receiving socket; each descriptor in flight counts against the sender's `RLIMIT_NOFILE`, and a send past it fails with `ETOOMANYREFS` unless the sender is root (from §18.6, holds `CAP_SYS_RESOURCE`), and a message with more than 253 descriptors (`SCM_MAX_FD`) fails with `EINVAL`, as unix(7) documents; a collector frees in-flight sockets that only a cycle of in-flight references reaches, so an in-guest test that sends a socket over itself and closes it, 10,000 times, finds the kernel's frame and open-file counts back at their baseline
- [ ] a socket's owner lock is dropped before a sender or receiver sleeps (DESIGN §2.1): in-guest, with one thread blocked in `recvmsg` on one end of a stream `socketpair`, another thread's `sendmsg` through the same descriptor completes, and the peer reads its bytes
- [ ] the same buffering machinery, and DESIGN §2.1's socket lock pair, reused for network sockets in phase 15, decided now rather than duplicated later: a syscall holds a socket's owner lock across its user copies and releases it before it sleeps; a sender appends to its peer's receive queue under the peer's SOCK spinlock; releasing the owner lock runs the socket's backlog step, which has nothing to drain until §15.5. `src/lock.rs` gains `RANK_SOCK` ahead of `RANK_HEAP`, and DESIGN §2.1's rank list and §2.7's I1 row name it, in the commit that adds the first socket

### 13.4 Shared memory
- [ ] POSIX `shm_open` backed by `tmpfs`, sized with §13.9's `ftruncate`
- [ ] anonymous shared mappings inherited across `fork`
- [ ] `memfd_create` for anonymous named regions, with `MFD_CLOEXEC`, `MFD_ALLOW_SEALING`, `F_ADD_SEALS`, and `F_GET_SEALS` as Linux defines them, since Wayland clients share buffers through sealed memfds
- [ ] correct refcounting so a region survives until the last mapper unmaps

### 13.5 futex
The §13.12 loom model and the in-guest tests below gate futex correctness; no futex benchmark gates this phase.

- [ ] `FUTEX_WAIT` and `FUTEX_WAKE` on a user address, with a hash table of wait queues; the key is computed, with its region lookup under the address-space lock, and that lock released before the bucket lock is taken; the word is read under the bucket lock through §12.5's non-faulting accessor, and on a fault the waiter drops the bucket lock, faults the page in with no lock held, and retries, as Linux's `futex_wait_setup` does (DESIGN §2.9 rule 4)
- [ ] private futexes keyed by (address space, virtual address); shared futexes by (backing object, offset), which is the inode for `shm_open`, `memfd_create`, and the unlinked tmpfs file behind an anonymous shared mapping (DESIGN §4.6). Never by physical frame: COW, page migration, and swap all move a frame under a sleeping waiter, and after `fork` unrelated processes share frames
- [ ] requeue and `WAKE_OP` for condition variables
- [ ] `set_robust_list` and `get_robust_list`, and the robust-list walk at thread exit that marks each held robust futex `FUTEX_OWNER_DIED` and wakes a waiter, so a dead owner's robust mutex returns `EOWNERDEAD`; musl registers a list on first robust lock, glibc for every thread
- [ ] the wait queue can record a lock owner, so §19.4 adds priority inheritance without changing this interface
- [ ] a timeout on every wait
- [ ] in-guest: a forked parent and child wait and wake on the same private address without seeing each other's wakes; two processes mapping one `shm_open` object do see them; a waiter survives a COW break on its page; the gate's cross-process mutex test

### 13.6 Event notification
- [ ] `ppoll`, and `poll` as its x86_64 entry point
- [ ] `pselect6`, and `select` as its x86_64 entry point, for compatibility
- [ ] `epoll` (`epoll_create1`, `epoll_ctl`, `epoll_pwait`) with edge and level triggering, since `poll` is O(n) per call and that becomes the bottleneck; `struct epoll_event` is packed on x86_64 only (12 bytes, 16 on aarch64), and a host test pins both; `EPOLL_CTL_ADD` fails with `ELOOP` when the add would close a loop of epoll instances or nest them deeper than epoll_ctl(2) allows, so readiness propagation stays bounded on the 16 KiB kernel stack, and with `ENOSPC` past `fs.epoll.max_user_watches`, whose default follows Linux's formula
- [ ] `eventfd2`, `signalfd4`, `timerfd_create` and its `settime`/`gettime`
- [ ] one internal readiness and wait-queue mechanism underneath all of them
- [ ] in-guest: 200 descriptors, within the Phase 10 per-process limit, with a handful ready, asserting the wakeup count

### 13.7 TTY and job control
- [ ] a TTY layer between the console and processes, with a line discipline
- [ ] one TTY per console backend, each with its own input queue: the serial port as Linux names it (`/dev/ttyS0`, the 16550 on x86_64; `/dev/ttyAMA0`, the PL011 on aarch64) and `/dev/tty1` over the framebuffer with the PS/2 or virtio-input keyboard. §5.3's merged input stops reaching userspace. `/dev/console` resolves to one of them for init's stdio, serial by default, and `/dev/tty` becomes the controlling-terminal alias. Kernel log lines and markers keep fanning out to every backend; on the serial TTY they stay framed, and user output and the TTY's echo of input stay escaped (DESIGN §2.6)
- [ ] `/sbin/init` starts `/bin/sh` on the serial TTY and a second one on `/dev/tty1`, each opened by name in its own session (`setsid`, then `TIOCSCTTY`) with that TTY as its controlling terminal, so both have job control; §14.3's gettys replace them
- [ ] the e2e console-input check (DESIGN §8.3) follows the split: the harness types the `sendkey` line into the `/dev/tty1` shell with its output redirected by this section's `>` to `/dev/console`, the serial TTY by default (the harness's `sendkey` table gains `>` and `/`); DESIGN §8.3 updated in the same commit
- [ ] in-guest: bytes injected on serial RX reach only the serial TTY's reader (`/dev/ttyS0` or `/dev/ttyAMA0`), and a keypress reaches only `/dev/tty1`'s
- [ ] canonical mode: line buffering, erase, kill, EOF
- [ ] raw mode, and the `termios` interface to switch between them
- [ ] control character handling generating signals: ctrl+C, ctrl+Z, ctrl+\. The line discipline's input side runs in the input device's threaded bottom half, or in the thread that polls a UART without a receive interrupt, never in a hard-IRQ top half (DESIGN §2.1, §5.4), and it signals the foreground group through the path `kill(-pgid, s)` takes
- [ ] sessions, process groups, controlling terminal, foreground group: `setsid`, `setpgid`, `getpgid`, `getsid`, `TIOCSPGRP` and `TIOCGPGRP`; a process group or a session keeps its id in use until its last member leaves (DESIGN §2.11 rule 4), and a host test with `pid_max` set to 16 reaps a session leader while its session has a member and finds its id skipped at the wrap
- [ ] `wait4` with `WUNTRACED` and `WCONTINUED`, and `waitid`
- [ ] `kill` and `wait4` take Linux's process-group arguments: both read `pid` as a sign-extended 32-bit `pid_t`; `kill(pid, 0)` checks existence and permission; `kill(0, s)`, `kill(-pgid, s)`, and `kill(-1, s)` signal the caller's group, that group, and every process the caller may signal except pid 1 and the caller; `wait4(0)` and `wait4(-pgid)` wait on a group; `wait4` returns `EINVAL` for an option bit outside `WNOHANG`, `WUNTRACED`, `WCONTINUED`, `__WALL`, `__WCLONE`, and `__WNOTHREAD`, and fills `*rusage` when the pointer is not null; a signal to a group or to `-1` takes the process table's lock once per process, resuming by pid, so no IF=0 stretch walks the whole group (DESIGN §2.9 rule 2) (F149)
- [ ] `SIGTTIN` and `SIGTTOU` for background access
- [ ] pseudo-terminals (`/dev/ptmx` and `/dev/pts`), which the terminal emulator in phase 16 and `sshd` in phase 15 require
- [ ] hangup, as POSIX `_exit` and Linux define it: a session leader's exit sends `SIGHUP` to its controlling terminal's foreground group and detaches the terminal from the session; an exit that leaves a process group orphaned while a member is stopped sends that group `SIGHUP` then `SIGCONT`; closing a pty's last master descriptor sends the slave's session leader and foreground group `SIGHUP` then `SIGCONT`, and reads on the slave then return 0. In-guest: a stopped background job left by an exiting session leader receives both signals, a foreground job receives `SIGHUP`, and a shell on a pty slave exits when the master closes
- [ ] a TTY's read and write locks are separate (DESIGN §2.1): in-guest, with one thread blocked in `read` on a pty slave, another thread's `write` through the same descriptor completes and its bytes reach the master
- [ ] window size and `SIGWINCH`
- [ ] the §10.5 `/bin/sh` gains `|`, `<`, `>`, `&`, one process group per job, terminal handoff, `fg`, `bg`, and `jobs`, which this phase's gate uses; §14.5's POSIX shell replaces it, and §14.3's harness root image keeps its `shell ready` role

### 13.8 Full signals
- [ ] the full signal set with correct default actions
- [ ] stop and continue lose no wakeup with user threads on several CPUs: an in-guest test sends `SIGSTOP` then `SIGCONT` 10,000 times from one user process to another running on a different CPU, and after each pair the target runs again; §10.6's stop-wait box made the check and the wait one SCHED section (F033)
- [ ] `rt_sigaction` with `SA_RESTART`, `SA_SIGINFO`, and an alternate stack (`sigaltstack`); the kernel `struct sigaction` per architecture as Linux defines it
- [ ] `rt_sigaction` honors `SA_RESTORER`. On x86_64 it is the only return path: a handler installed without it is not run, and the process gets `SIGSEGV`, as on Linux; nothing is ever written to the NX stack as code. On aarch64, a handler without a restorer returns through a read-only, user-executable trampoline page the kernel maps into every process at `exec`, so glibc-static binaries work; the page holds Linux's `mov x8, #139` and `svc #0`, the pair libgcc's and LLVM's unwinders match to step through a signal frame, and §13.10's vDSO exports it as `__kernel_rt_sigreturn`. The user crate gains `sigaction`, `sigprocmask`, and a restorer stub per architecture, which the gate's handler test uses
- [ ] `rt_sigprocmask`, `rt_sigpending`, `rt_sigsuspend`, `rt_sigtimedwait`
- [ ] handled signals take the §12.6 `SIGKILL` path: a signal is acted on at syscall exit and at interrupt and exception exit to ring 3 or EL0, a signal sent to a thread running on another CPU sends that CPU a reschedule IPI (an SGI on aarch64), and a thread in an interruptible wait is woken
- [ ] delivery builds a Linux `rt_sigframe` on the user stack (`siginfo`, `ucontext` with `uc_mcontext`, and the FP state), runs the handler, and returns through `rt_sigreturn`; the `ucontext` layout per architecture is pinned by a host test, since musl reads it
- [ ] SYSCALL.md §1 records the x86_64 user FP format this phase uses: the 512-byte FXSAVE image with `CR4.OSXSAVE` clear (§11.1), or XSAVE with XCR0 limited to x87, SSE, and AVX and the per-thread area sized from CPUID leaf 0Dh; the `XSTATE_BV` and `XCOMP_BV` checks below and §17.4's `NT_X86_XSTATE` apply only under XSAVE, and under FXSAVE `NT_X86_XSTATE` returns `ENODEV`, as Linux does on a CPU without XSAVE (F130)
- [ ] x86_64 frame: on the current stack it begins at least 128 bytes below the interrupted RSP (the red zone), on the alternate stack at that stack's top, and RSP+8 is 16-byte aligned at handler entry. The frame carries the FPU image, with any state still live in the registers saved first. aarch64 frame: the FPSIMD record, SP 16-byte aligned
- [ ] signal delivery (the `sa_handler` and `sa_restorer` addresses) and `rt_sigreturn` (the whole restored frame) validate the user context before the return to userspace. On x86_64: RIP canonical and below `USER_MAP_END`, CS and SS forced to the user selectors, RFLAGS masked to user-settable bits (never IOPL, NT, VM, or RF from the frame), MXCSR masked to `MXCSR_MASK`, and in the XSAVE header `XSTATE_BV` a subset of the user features in XCR0, `XCOMP_BV` zero, and the reserved bytes zero, as Linux's `validate_user_xstate_header` requires, before the image reaches `fxrstor`/`xrstor`; a fault on that restore, wherever the kernel performs it, goes through an exception-table fixup that loads the initial FP state and delivers `SIGSEGV`. On aarch64: SPSR forced to EL0t, with only NZCV and the other user-settable bits taken from the frame
- [ ] in-guest, per architecture: `rt_sigreturn` with a non-canonical PC, kernel selectors or EL1 mode, IOPL set, reserved MXCSR bits, or an XSAVE image with an `XSTATE_BV` bit outside XCR0, a nonzero `XCOMP_BV`, or a nonzero reserved header byte gets `SIGSEGV` and the kernel stays up; a handler that interrupts a leaf function holding live data in its red zone leaves that data intact
- [ ] real-time signals with queueing, since standard signals coalesce and that surprises people; queued signals count against `RLIMIT_SIGPENDING` as getrlimit(2) documents, and a send past it that would queue fails with `EAGAIN`
- [ ] interaction with blocking syscalls: interrupt with `EINTR` or restart, per `SA_RESTART`
- [ ] per-thread signal masks with process-directed signals delivered to an eligible thread
- [ ] the `core` default action writes an ELF core in Linux's per-architecture note layouts: one `NT_PRSTATUS` per thread, `NT_PRFPREG`, `NT_PRPSINFO`, `NT_SIGINFO`, `NT_AUXV`, and `NT_FILE`, plus the writable and anonymous mappings, to the path a `core_pattern=` option on the §10.2 kernel command line names (`%p` and `%e` expanded; `core` in the working directory when absent, as on Linux), under `RLIMIT_CORE`. A process §13.9 marks not dumpable leaves none. The core file is created with the dumping process's fsuid and fsgid, mode 0600, and `O_NOFOLLOW`, and is never written through a symlink, a file that is not regular, a file with more than one link, or a file another uid owns, as core(5) documents; an in-guest test plants a symlink and a hard link, as uid 1000, at the core path of a root process that then dumps, and finds both targets unchanged and no core written. Host `gdb` and `lldb` open it against the binary
- [ ] utest runs attach a scratch FAT32 image as a second virtio-blk disk, which `/sbin/init` mounts at `/core` with §13.9's `mount`, and set `core_pattern=/core/%e.%p`; after QEMU exits the harness copies each core off it and prints a host backtrace beside the failing utest (`gdb-multiarch` in CI, `lldb` on the macOS dev host)

### 13.9 POSIX floor
- [ ] a tracked list of the syscalls needed to build and run the target software set: every number in a pinned copy of musl's `arch/<arch>/bits/syscall.h.in` for each architecture has a row in the §10.5 table with the same number, marked implemented, partial, or `ENOSYS` by design with the fallback its known callers take, and `make check` fails on a missing row, a differing number, or a row whose number musl does not name (SYSCALL.md §8); `docs/SYSCALL.md` §3 renders the status, and the §13.11 runner lists in its job summary every `ENOSYS` the corpus hits
- [ ] every call in this phase lands under its asm-generic name, which x86_64 also has; the legacy names are x86_64-only entry points onto it, as §11.6 does for `openat`, `dup3`, and `clone`: `faccessat` for `access`, `fchmodat` for `chmod`, `fchownat` for `chown`, `newfstatat` for `stat` and `lstat`, `readlinkat` for `readlink`, and the §13.2 and §13.6 pairs
- [ ] `getcwd`, `chdir`, `fchdir`, `faccessat`, `fchmodat`, `fchownat`, `umask`, `utimensat`
- [ ] the VFS takes names of up to 255 bytes and paths of up to 4096 bytes including the terminating NUL, Linux's `NAME_MAX` and `PATH_MAX`, so every path call returns `ENAMETOOLONG` exactly where Linux does, and a filesystem with a shorter limit (vibefs v1's 64 bytes, FAT's 255 UTF-16 code units) returns it from its own `create` and `lookup` only; the path buffer is allocated per call, fallibly (DESIGN §4.4), not on the 16 KiB kernel stack. Today `fs::MAX_NAME` is 64 and a path of 256 bytes or more fails, so a Linux program that creates a 100-byte name or passes a 300-byte path fails where it succeeds on Linux (SYSCALL.md §2). Host tests create and look up a 255-byte name on tmpfs and resolve a 4095-byte path; a 256-byte name and a 4097-byte path return `ENAMETOOLONG`
- [ ] `openat` with `O_CREAT` creates the file with `mode & ~umask` on vibefs and tmpfs, where `open` ignores `mode` today (F149)
- [ ] `newfstatat`, `readlinkat`, `openat` with any `dirfd`, `pread64`, `pwrite64`, `readv`, `writev`, which every coreutil calls before anything else; `getdents64`, `fstat`, and `nanosleep` came with §10.5
- [ ] `fcntl` beyond `F_GETFD` and `F_SETFD`: `F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFL`, and `F_SETFL`; POSIX record locks (`F_SETLK`, `F_SETLKW`, `F_GETLK`), OFD locks, and `flock`, with `EDEADLK` when an `F_SETLKW` would close a wait cycle, since parallel `make`, `cargo`, and package managers rely on them for correctness
- [ ] `mkdirat`, `unlinkat`, `renameat`, `renameat2`, `linkat`, `symlinkat`, `mknodat`, `truncate`, `ftruncate`, `fsync`, `fdatasync`, `sync`, `statfs`, `fstatfs`, `mount`, `umount2`, `sethostname`, `settimeofday`, and `clock_settime`, with the x86_64 legacy names (`mkdir`, `rmdir`, `unlink`, `rename`, `link`, `symlink`, `mknod`) as entry points onto them; §13.2's named pipes, §13.4's `shm_open`, §14.3's init, §14.4's coreutils, §14.6's installs, and §15.8's SNTP client need them; the calls that change system state, and `mknodat` of a device node, are root-only under the enforcement below
- [ ] `renameat` takes its two directories in DESIGN §2.1's order, ancestor before descendant and otherwise address order, after the volume's rename lock; in-guest, on two CPUs, 10,000 rounds of renaming a file back and forth between `/t/a/b` and `/t/a` race `unlinkat(AT_REMOVEDIR)` of `/t/a/b`, which locks `/t/a` and then `/t/a/b` before it finds the directory not empty, on tmpfs and on vibefs, and neither side deadlocks
- [ ] a `make check` script reads the kernel's `-Z emit-stack-sizes` section and fails when a function reachable from a syscall or a shell command has a frame over the bound DESIGN §3.5 records in the same commit; the bound leaves room for the deepest hard-IRQ top half and its entry frame, since interrupts land on the interrupted thread's kernel stack (DESIGN §2.2) and syscall bodies take them (DESIGN §2.9) (F058)
- [ ] partition tables are validated before `mount` can reach a partition: the GPT header's `MyLBA` equals the LBA it was read from, every GPT entry lies within `FirstUsableLBA..=LastUsableLBA`, and no GPT or MBR entry overlaps another entry, the MBR, the GPT headers, or the entry arrays; a refused entry is logged and gets no child device; host tests cover each case (F117)
- [ ] vibefs `truncate` past the 128-byte inline limit spills the inline data to an extent while `size` still holds the old value; `read` and `spill_inline` bound inline copies by `inline_len` and return `Corrupt` on a mismatch, and `unpack_inode` and fsck reject `F_INLINE` with a size over 128; a host test truncates a 3-byte inline file to 256 bytes and reads 253 zero bytes after the data (F062)
- [ ] `truncate` and `ftruncate` zero what a later extension exposes: a FAT shrink zeroes the last kept cluster from `new % cluster_bytes` to its end, a vibefs inline shrink zeroes `inline_data[new..]`, and a vibefs extent shrink rewrites the last kept block (CoW) with a zeroed tail; host tests write 400 bytes, truncate to 10, extend to 300, and read zeros from byte 10 on FAT, do the same with 3000, 200, and 2000 bytes on a vibefs extent file, and with 100, 10, and 120 bytes on a vibefs inline file (F125)
- [ ] FAT directory walks fail on errors: `read_dirent` returns `Io` and `Corrupt` instead of end-of-directory, so `lookup`, `readdir`, and `dir_empty` report the error, `unlinkat(AT_REMOVEDIR)` never frees a non-empty directory, and `O_CREAT` never adds a duplicate name; a host test injects an I/O error mid-directory (F124)
- [ ] `FatVol::walk` resolves `..` from the directory's own `..` entry (cluster 0 is `root_clus`) instead of a 16-entry ancestor stack that drops deeper pushes; a host test walks `..` in a 20-deep tree (F124)
- [ ] the §12.5 crash-state workloads run again from a user program through `fsync`, `renameat`, and `sync`, on the volatile-cache disk, on both architectures on the nightly job
- [ ] `/dev/kmsg` in devfs, so `dmesg` reads the kernel log; only root reads it (from §18.6, a process with `CAP_SYSLOG`), which is Linux's behaviour with `kernel.dmesg_restrict` at 1, since the log can carry kernel addresses; a read as uid 1000 fails with `EPERM`
- [ ] credentials in userspace: the §9.5 uid and gid gain saved set-IDs and a supplementary group list, inherited across `fork` and `execve`; `getuid`, `geteuid`, `getgid`, `getegid`, `getresuid`, `getresgid`, `getgroups`, `setuid`, `setgid`, `setreuid`, `setregid`, `setresuid`, `setresgid`, `setgroups` over them. Each checks privilege as Linux does: effective uid 0 sets any id, any other caller only switches among its own real, effective, and saved ids, and `setgroups` is root-only
- [ ] uid and gid enforced (moved from §18.6): permission bits and ownership checked on every path walk and file operation; the set-user-ID and set-group-ID bits honored by `execve`, with `AT_SECURE` set so §14.2's dynamic linker ignores `LD_PRELOAD` and `LD_LIBRARY_PATH`; a write or truncate by a non-root caller (from §18.6, one without `CAP_FSETID`) clears the set-user-ID bit, and the set-group-ID bit when group-execute is set or the file's group is neither the caller's effective gid nor one of its supplementary groups, and a `chown` of a non-directory clears the set-user-ID bit for every caller, root included, and the set-group-ID bit under that same group rule, applied to the file's group before the change, for every caller but root, as Linux does since 6.2; `kill` and `tgkill` to another user's process refused; `/proc/<pid>/environ`, `auxv`, `maps`, and `fd/` readable only by the owner or root, and only by root while the process is not dumpable. A process whose effective uid or gid changes, through a set-ID `execve` or one of the `set*id` calls above, is not dumpable until an `execve` leaves its real and effective ids equal, as under Linux's default `suid_dumpable`; FAT mounts given one owner and a file and directory mode from `uid=`, `gid=`, `fmask=`, and `dmask=`. Root is effective uid 0 until §18.6's capabilities split it
- [ ] in-guest, as uid 1000: reading a root-owned mode-0600 file and writing another user's file return `EACCES`, `setuid(0)` returns `EPERM`, a set-user-ID-root test binary runs with euid 0 and sees `AT_SECURE` 1, and `kill` of a root-owned process returns `EPERM`
- [ ] `procfs` backed by the process table: `cmdline`, `status`, `maps`, `fd`, `stat` per pid; the §10.5 `/bin/sh` `ps` built-in reads it and syscall 500 is deleted; a file that reads another process's address space (`maps`) pins it with a get-unless-zero `users` reference (DESIGN §2.11) for the read only
- [ ] `getrlimit` / `setrlimit` (`prlimit64`), `getrusage`; `RLIMIT_NOFILE` lifts a process from the default of 256 descriptors (Phase 10) to a hard cap in the `limits` module, since toolchains open more than 256 files
- [ ] `uname`, `sysinfo`, `gettimeofday`, `clock_gettime`, `clock_getres`, and `clock_nanosleep` over Linux's clock ids: `REALTIME`, `MONOTONIC`, `MONOTONIC_RAW`, `BOOTTIME`, the `_COARSE` variants, and the process and thread CPU-time clocks, including the ids that encode a pid or tid, as `clock_getcpuclockid` and `pthread_getcpuclockid` build them
- [ ] `timer_create`, `timer_settime`, `timer_gettime`, and `timer_delete` with `SIGEV_SIGNAL` and `SIGEV_THREAD_ID` (musl builds `SIGEV_THREAD` on it), and `setitimer` and `getitimer`, with `alarm` as an x86_64 entry point onto `setitimer`, on one timer queue shared with §13.6's `timerfd`, whose expiries run as DESIGN §2.2 timer callbacks
- [ ] `ioctl` with a registry rather than a growing match arm
- [ ] `prctl` through an option table: an option not in it returns `EINVAL`, as Linux does for an unknown option, and each entry names the §13.11 corpus program or the line that needs it; §18.6 and §23.1 add their options to the same table
- [ ] `ENOSYS` for the unimplemented, logged once per syscall number, so a port's failure is immediately legible

### 13.10 Linux ABI fidelity
§11.6, §13.6, and §13.8 pin `stat`, `epoll_event`, `sigaction`, the signal frame, and `ucontext`. This covers the rest of what an unmodified static Linux binary reads, so it sees what it would see on Linux. `PT_INTERP` and shared objects stay §14.2.

- [ ] static-PIE executables (moved from §14.2): `execve` maps an `ET_DYN` image with no `PT_INTERP` at a page-aligned bias it chooses, `AT_PHDR` and `AT_ENTRY` carry the bias, and the image relocates itself, as on Linux. `src/elf.rs` stops refusing `ET_DYN` without an interpreter, and SYSCALL.md §7 says so. Host tests cover the bias arithmetic; an in-guest test runs a static-PIE user-crate program, whose `_start` applies its own relative relocations, on both architectures. Alpine's static `busybox` and Rust's `x86_64-unknown-linux-musl` output are static-PIE; Rust's `aarch64-unknown-linux-musl` output on the pinned nightly is a static `ET_EXEC`
- [ ] `execve` of a file that begins with `#!` runs the named interpreter as Linux's `binfmt_script` does: from a first line within 256 bytes (`BINPRM_BUF_SIZE`), the interpreter path and the rest of the line, trimmed, as at most one argument; the new argv is the interpreter, that argument if present, the path passed to `execve`, then the original `argv[1..]`. The script's set-user-ID and set-group-ID bits are ignored and the interpreter's own apply under §13.9, as on Linux. A file that is neither ELF nor `#!` returns `ENOEXEC`, and a chain of more than five rewrites, Linux's `exec_binprm` limit, returns `ELOOP`; SYSCALL.md §2 and §7 say so. The parser is host-tested; an in-guest test on both architectures execs a script whose interpreter is a user-crate program that prints its argv, a chain of five scripts, each the interpreter of the one before, that runs, a chain of six that returns `ELOOP`, and a set-user-ID-root script run as uid 1000 that still sees euid 1000. musl's `execvp` has no `ENOEXEC` fallback to `sh`, and `apk` runs package scripts and triggers through `execve`
- [ ] the auxiliary vector Linux passes: `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_BASE`, `AT_FLAGS`, `AT_ENTRY`, `AT_UID`, `AT_EUID`, `AT_GID`, `AT_EGID`, `AT_SECURE` (§13.9), `AT_RANDOM` (16 bytes from `RDRAND`, `RNDR`, or virtio-rng through the `/dev/random` source, never the TSC or a fixed-seed generator), `AT_HWCAP`, `AT_HWCAP2`, `AT_PLATFORM`, `AT_EXECFN`, `AT_CLKTCK`, `AT_MINSIGSTKSZ`, and `AT_SYSINFO_EHDR`. `AT_HWCAP` on aarch64, and XCR0 on x86_64, advertise only register state the kernel saves and restores. On aarch64, `AT_HWCAP` carries `HWCAP_EVTSTRM` for DESIGN §11.4's event stream, and no `HWCAP_CPUID` until §23.1 emulates EL0 `mrs` of the ID registers. A user-crate program that prints its auxv runs in the §13.11 differential runner (F140)
- [ ] every thread the kernel creates starts from the §11.6 `execve` FP image, held as a constant, instead of `FPU_TEMPLATE`, the `fxsave64` taken after `fninit` at boot, which keeps the loader's MXCSR and ST and XMM contents; the `Fxsave::empty()` fallback (FCW 0, MXCSR 0) that `init_bootstrap` gets before `init_fpu` runs is gone; a host test pins the image bytes, and a user-crate program that prints its initial MXCSR and FCW runs in the §13.11 differential runner (F129)
- [ ] a vDSO on both architectures, mapped at `exec` with a read-only data page the §2.7 seqlock publishes, and built from the same portable time code: the calls and symbol names Linux's vDSO exports (`__vdso_clock_gettime` and relatives at version `LINUX_2.6` on x86_64; `__kernel_clock_gettime` and relatives, and `__kernel_rt_sigreturn`, at `LINUX_2.6.39` on aarch64), since musl and Go look them up by name and version. It computes the time with the same function as `now_ns`, from one published record, and falls back to the syscall when user mode cannot read the counter or the §10.7 warp test saw a backward step (F027)
- [ ] in-guest: 10,000 `clock_gettime(CLOCK_MONOTONIC)` calls through the vDSO leave the calling process's §10.7 syscall count unchanged, and interleaved with syscall reads they never run backwards, on both architectures (F150)
- [ ] `AddressSpace::map_anon`'s rollback removes only leaves it installed: on `AlreadyMapped` it frees its own frame and leaves the existing leaf in place, and `user_frames` drops only for pages it counted. This lands before the vDSO pages and §13.8's aarch64 trampoline page add user leaves outside `map_anon`. A host test maps over a leaf that no `Region` covers and finds the leaf, its frame, and the frame count unchanged (F102)
- [ ] `sched_getaffinity` and `sched_setaffinity` over a per-thread CPU mask sized from the boot CPU count, which replaces §4.8's `CpuAffinity::{Any, Pinned(cpu)}` (it cannot hold a set) and which placement honors, and `getcpu`, since musl's `sysconf(_SC_NPROCESSORS_ONLN)`, `nproc`, Rust's `available_parallelism`, and Go's `GOMAXPROCS` count CPUs through it, and a build that sees one CPU runs serially
- [ ] extending §13.9's procfs: `/proc/self` and `/proc/thread-self`; per pid `exe`, `cwd`, and `root` symlinks, `fd/<n>` links that open the descriptor's own file rather than re-walking a path, `auxv`, `environ`, and `task/<tid>/comm`, which Rust's `current_exe`, the sysroot discovery in `rustc` and `clang`, and musl's `ttyname`, `fexecve`, and `pthread_setname_np` use
- [ ] one host-tested table of Linux struct sizes and field offsets per architecture, whose values cite the uapi header they came from, covering every struct the kernel reads or writes: the §11.6, §13.6, and §13.8 pins move into it, and it adds `termios` (the kernel's 36-byte layout, not libc's larger one), `winsize`, `timespec`, `timeval`, `rlimit`, `rusage`, `utsname`, `sysinfo`, `pollfd`, `iovec`, `linux_dirent64`, `sockaddr_un`, `msghdr`, `cmsghdr`, `ucred`, `siginfo_t`, `signalfd_siginfo`, `itimerspec`, `itimerval`, `sigevent`, `flock` (the `fcntl` lock record), `robust_list_head`, `stack_t`, and `statfs`; a struct a new syscall reads or writes enters the table in the same commit
- [ ] `ioctl` request numbers and argument layouts are Linux's for every request implemented, each pinned by a host test: `TCGETS`, `TCSETS`, `TCSETSW`, `TCSETSF`, `TIOCGWINSZ`, `TIOCSWINSZ`, `TIOCSCTTY`, `TIOCNOTTY`, `TIOCGPGRP`, `TIOCSPGRP`, `TIOCGSID`, `TIOCGPTN`, `TIOCSPTLCK`, `FIONREAD`, `FIONBIO`, `FIOCLEX`, and `BLKGETSIZE64`
- [ ] the `uname` policy in `docs/LINUX.md`, decided here: `sysname` is `Linux`, since vibeOS implements Linux's interfaces and libcs, runtimes, and `config.guess` take their Linux paths only on that name; `release` is the baseline's version with a `-vibeos` suffix (such as `6.12.0-vibeos`), so minimum-version checks pass and a program that wants to know can tell; `version` names vibeOS, its release, and the commit date from `SOURCE_DATE_EPOCH`, so the string is reproducible. Rejected: `sysname` `vibeOS`, which sends every tool that branches on it (Python's `platform.system()`, autoconf, CMake) to an unknown-OS fallback. The §13.11 runner's expected differences follow the policy

### 13.11 Conformance against Linux
The syscall numbers are Linux's on x86_64 (§9.3) and asm-generic on aarch64 (§11.6), so a static Linux binary is a test that needs no port, and Linux is the oracle. Test inputs are fetched by hash at test time, never committed, and never put in a shipped image.

- [ ] a corpus of unmodified static `*-linux-musl` binaries, built from pinned upstream release sources with Alpine's own toolchain in a digest-pinned Alpine container on each architecture's GitHub-hosted Linux runner: `busybox` in Alpine's `busybox-static` configuration, with its test suite; `toybox`; and LTP's `syscalls` cases for the calls this phase lands. Sources are fetched and checked by SHA-256; prebuilt packages are not used, since Alpine's mirrors drop superseded revisions. The corpus key hashes the sources' SHA-256s and the container digest. The nightly job builds a corpus only for a key that has none, and uploads it as assets of a `corpus-<key>` GitHub pre-release, with the source archives it was built from, which its GPL parts require. Every other run, the macOS dev host's included, downloads the corpus by that key and checks its SHA-256 instead of rebuilding
- [ ] a differential runner: each corpus command runs in the guest and on Linux, and stdout, stderr, exit status, and the resulting file tree must match, except differences `docs/LINUX.md` lists with the Linux behavior and the reason (pids, times, the §13.10 `uname` policy); each Linux result records the oracle kernel's release (`uname -r`), and a run whose oracle release differs from the previous run's says so in its job summary, so a runner image update is not read as a vibeOS regression
- [ ] the §11.6 EL0 and ring-3 environment case, built as a static `*-linux-musl` probe that prints which signal each operation raised, joins the corpus on both architectures, so a DESIGN §11.4 value that makes user code see something other than Linux's fails the differential runner unless `docs/LINUX.md` lists it
- [ ] Linux's result for each corpus case, recorded by the hosted runners and uploaded to the same `corpus-<key>` release, so a run on the macOS dev host compares against it
- [ ] a generator in the user crate emits syscall sequences over this phase's calls from the §10.5 table: files, directories, links, renames, descriptors and offsets, pipes, `fork` and `wait4`, and signals with default actions, sized within vibefs v1's limits (VIBEFS.md §3) until §14.8. The same static binary runs each sequence on Linux in a tmpfs directory and in the guest on vibefs and on tmpfs, recording every return value, errno, and the final file tree
- [ ] a divergence not listed in `docs/LINUX.md` fails; the sequence is minimized to the shortest one that still diverges and checked in as a `/bin/tests` regression
- [ ] busybox's own test suite runs in the guest with busybox as its shell, against a checked-in expected-failure list per architecture
- [ ] the LTP cases run one by one with pass, fail, and `TCONF` read from each exit status, against a checked-in expected-failure list per architecture whose entries name the missing call or the deliberate difference; a listed case that passes fails the run, so the list only shrinks
- [ ] after a case panics or hangs the kernel, the runner restores a QEMU snapshot taken at `shell ready`, so one bad case costs one restore, not the run
- [ ] a static Go program (pinned upstream Go toolchain, `CGO_ENABLED=0`, cross-built on any host) runs goroutines on every CPU with async preemption by `SIGURG`, timers, and the netpoller over a pipe, in a 4-CPU, 512 MiB guest under TCG, which checks §13.1's threads, §13.5's futexes, §13.6's `epoll`, and §13.8's frame rewrite together
- [ ] the corpus, the generator, busybox's suite, LTP, and the Go program run on the nightly job on both architectures

### 13.12 Lock-dependency validation and the futex model
The §4.7 rank check has six global ranks and skips rank 0, and from §10.3 it lets a rank be retaken only through `lock_nested`, whose pair order it cannot check, so it cannot see an AB-BA order between two locks of one rank. This phase adds many, each at the place DESIGN §2.1 gives it: the file description's position lock, pipe ends, socket pairs, and the TTY, session, and process-group locks.

- [ ] every `SpinMutex`, `BlockingMutex`, and `RwLock`, and each socket's owner lock, gets a lock class keyed by its initialization site, beside its DESIGN §2.1 rank, so every pipe's lock is one class; nesting two locks of one class takes an explicit subclass and a fixed order, such as address order for a socket pair, and for a two-directory `rename` ancestor before descendant, then address order (DESIGN §2.1); the classes carry DESIGN §2.1's two tiers and level 1's order (position lock, then stream lock, then namespace and inode locks), so a sleeping lock taken with a spinlock held, a level-1 lock taken with the address-space lock held, a stream lock taken with a namespace or inode lock held, a position lock taken with a stream, namespace, or inode lock held, and a user copy made while a level-2, 3, 3b, or 4 lock is held are reported the first time they happen; the reverse-map lock's class has two subclasses, a file mapping's taken before an anonymous object's. The same build reports a region's file or page-cache reference dropped with the address-space lock held, and gives direct reclaim's wait for submitted writes a class of its own outside the tier order (DESIGN §4.4 rule 3)
- [ ] a `lockdep` build records each held-class to acquired-class edge and reports the first cycle with both acquisition backtraces, the first time both orders occur in any run rather than when they collide in time
- [ ] the same build reports a class taken in IRQ context that is elsewhere held with interrupts enabled, and a sleep on a wait queue with a spinlock held
- [ ] the nightly job runs the in-guest ladder in this build on both architectures and dumps the edge graph, so a report names the path that closed the cycle
- [ ] an in-guest test nests two device-rank locks with `lock_nested` in both orders, one after the other on one CPU, and passes only when the checker reports it (F108)
- [ ] the §13.5 futex table, wait, wake, and requeue in the portable half with a loom model (§10.8): a wake racing a timeout, a requeue racing a wake, a waiter whose page breaks COW, and a waiter whose word read faults, drops the bucket lock, and retries while a wake runs; a mutant that checks the futex word outside the bucket lock must fail the model
- [ ] `thread_init::with_sched` holds an `InterruptGuard` from before it takes `SCHED` through its `place_ready` loop, so a caller that blocked itself in the closure cannot be preempted before the wakes it recorded are placed, and `Condvar::wait_until` no longer strands the mutex waiter it woke. This lands before the first non-test `Condvar` caller. An in-guest test has the notifier queued on the mutex when the waiter calls `Condvar::wait`, and both finish. DESIGN §9.4's Condvar pitfall drops its `with_sched` sentence (F034)
- [ ] an `RwLock` writer that gives up after a wait wakes the readers parked behind it on every return path: the `after_writer_wait_timeout` wake runs before any `Err` return that follows a wait, including the `past(d)` return after `drop_read` woke the writer and a reader barged in; a host-model test runs that woken, barged, past-deadline sequence and finds no reader left parked (F036)

### 13.13 Syscall fuzzing
Moved from §18.5: Phases 12 and 13 build the syscall surface and rewrite user-pointer handling, and a fuzzer finds those bugs while the phase that wrote them is open. §18.5 moves on to syzkaller with kernel coverage.

- [ ] the §10.5 table gains an argument kind per parameter: descriptor, user pointer with the index of its length argument, path, flag set with its valid mask, pid, signal, or plain integer; the fuzzer generates from it and the §9.3 syscall trace decodes with it
- [ ] a fuzzer in the user crate generates calls from those kinds: live, closed, and wrong-type descriptors; pointers into unmapped, read-only, kernel, COW-shared, and not-yet-faulted pages; lengths across page boundaries; every flag bit; several threads at once; its descriptors 0 to 2 are a pipe its runner reads, so random writes neither flood the serial console nor mix with the lines the harness reads
- [ ] every sequence derives from one seed, printed on serial before it runs, so a crash replays from its seed as a utest, and each replay is checked in
- [ ] after each run the §12.1 per-category frame counts, measured from the §10.2 quiescent baseline, and the process, thread, and descriptor tables return to baseline, so a leak fails the way a panic does (F074)
- [ ] the nightly job runs it one hour per architecture in a build with the §12.1 KASAN and §13.12 lock-dependency checks on
- [ ] the weekly job also runs it one hour per architecture in the §12.1 `debug_mm` build, where a run fails when a buffer the kernel copied out holds 8 or more consecutive bytes of that build's poison pattern, which the fuzzer never writes; only an uninitialized heap block or frame reaching user memory puts it there, which `unsafe` code can do past §10.6's padding rule

---

# Era III. Platform

Where it stops being a kernel demo and becomes something you can use. Nothing here has a canonical
right answer, which is the interesting part.

Phases 15, 16, and 17 all start from 14, and each checks its Linux interfaces with unmodified tools from
§14.9's Alpine userland. Phase 16 needs nothing else, so self-hosting does not wait for a compositor.
Phase 17's build loop runs offline from a vendored tree on a disk image, so only its network lines
(`cargo` fetching, `git` remotes, `vim` over `sshd`) wait for 15; the rest starts when 14 closes. Phase 17 runs upstream
toolchains (§17.7); rebuilding them from source is Phase 24, and glibc userlands are Phase 23. The
numbering is a reading order, not a build order.

## Phase 14: Userspace

**Goal.** A real userspace: a C library, a dynamic linker, an init system, crypto primitives, a root
filesystem on disk, and enough utilities that the shell is useful. It also runs an unmodified Alpine Linux
userland (§14.9), which Phase 17's toolchains come from.

**Unlocks.** Porting software instead of writing everything. Logins, and packages that can be trusted. A
root filesystem large enough for the Phase 17 toolchains. Unmodified Linux binaries as the test tools of
Phases 15 and 16 and the toolchains of Phase 17.

**Architectures.** Both. musl's x86_64 and aarch64 ports bind to the one generated table (§11.6). The
dynamic linker's relocation types and TLS are per architecture. Alpine publishes both architectures, so
§14.9 is both.

**Exit gate**
- [ ] a C program cross-compiled on the host with the §14.1 sysroot runs correctly on both architectures
- [ ] a dynamically linked binary against a shared libc runs, and `ldd` lists its dependencies
- [ ] init starts services from configuration, restarts a crashed one, and reaps orphans
- [ ] `login` on the serial console accepts a user from the hashed shadow file and refuses a wrong password, and `id` in the new session reports that user's uid, gid, and groups; that user cannot read `/etc/shadow` or write another user's file, and `passwd` run by that user changes only that user's entry
- [ ] a shell script with pipes, redirection, variables, conditionals, and loops runs
- [ ] `/bin/tests` (§10.5) and `libc-test` (§14.1), minus the cases on §14.1's checked-in expected-failure list, pass inside the VM on both architectures, run automatically in CI
- [ ] a package installs, upgrades, and removes cleanly with file conflict detection, and a package with a bad signature is refused
- [ ] two boots of one image with one QEMU command line get different first 32 bytes from `getrandom`, on both architectures
- [ ] the §14.7 primitives pass their published test vectors in-guest on both architectures
- [ ] the system boots with root on a vibefs v2 disk image built from the §14.6 packages, on both architectures, and this gate runs from it
- [ ] on both architectures, in a 1 GiB, 2-CPU guest under TCG, inside a `chroot` of Alpine's pinned `minirootfs` on vibefs v2: `apk add python3 git build-base clang lld` from the §14.9 mirror succeeds with signatures verified, `make test-harness` passes on a copy of the tree, `git` commits to a local repository, and `clang` compiles and links a C program that then runs
- [ ] every third-party source the tree builds, other than Rust crates and Limine, has a `ports/` manifest with an allowed license, enforced by `make check`, and the scheduled job verifies every archive hash and patch series (§14.10)
- [ ] tag `phase-14` and release `v0.14.0`

### 14.1 libc
- [x] decided in Phase 10: the Rust user runtime from §10.5 is the native library for everything vibeOS ships; musl is ported for the C surface, the libc that §14.9's Alpine userland and §17.7's toolchains also run on. Both bind to the one generated syscall table.
- [ ] upstream musl, unmodified and pinned by version and SHA-256 under §14.10: its `crt1`, `__libc_start_main`, `syscall` shim, `errno`, and `__set_thread_area` run over §13.1's TLS as they do on Linux; no patch is carried, since a difference that would need one is a kernel bug; `libc-test` as the conformance suite
- [ ] a host cross toolchain: `make sysroot` builds, for each architecture, musl, our headers, and compiler-rt's builtins and crt objects (aarch64 `long double` is binary128, so musl's `printf` alone needs `__multf3` and relatives). A per-triple clang config file sets `--sysroot`, `-resource-dir`, `-fuse-ld=lld`, and `--rtlib=compiler-rt`, so `clang --config=<it> --target=<arch>-linux-musl` builds and links C for vibeOS on the host. That needs an LLVM clang and `ld.lld`, which on macOS means Homebrew `llvm` and `lld`, since Apple's clang ships neither; README and AGENTS.md gain them in the same commit. The Linux syscall numbers and struct layouts (§11.6) are what make that triple work
- [ ] the §13.9 syscall-table check reads musl's headers from this pinned musl instead of its own copy
- [ ] musl's `mallocng` over `brk` and §12.4's `mmap`, `mremap`, and `madvise`; §18.4 hardens it
- [ ] musl's stdio, string, math, `pthreads` (over §13.1 threads and §13.5 futexes, cancellation over §13.8 signals), `setjmp`, `dlopen`, and locale pass `libc-test` on both architectures. Each failure is fixed in the kernel in this phase, or recorded on a checked-in expected-failure list with its reason, or added as an open box to the phase that lands it

### 14.2 Dynamic linking
- [ ] the kernel side: `execve` of an image with `PT_INTERP` also maps the named interpreter at a load bias it chooses and enters at the interpreter's entry point; the main program is `ET_EXEC`, or `ET_DYN` placed at a bias as §13.10 places static-PIE. `AT_BASE` carries the interpreter's base, and `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, and `AT_ENTRY` describe the main program with its bias. `src/elf.rs` stops refusing `PT_INTERP`, and SYSCALL.md §7 says so. Host tests cover the interpreter's bias arithmetic; an in-guest test runs an interpreted PIE and an interpreted `ET_EXEC` binary on both architectures
- [ ] musl's dynamic linker, which is `libc.so` itself, static and self-relocating: `DT_NEEDED` and search paths, `RELA`, `JMPREL`, and `RELR`, and symbol resolution with the correct scope and interposition order
- [ ] binding is immediate, as musl does it; `RTLD_LAZY` defers only unresolved symbols, and there is no lazy PLT binding
- [ ] TLS: initial-exec and the dynamic models, through `__tls_get_addr` on x86_64 and TLS descriptors (`R_AARCH64_TLSDESC`) on aarch64, where musl binds a module loaded at startup to `__tlsdesc_static` and a `dlopen`ed one to `__tlsdesc_dynamic`; an in-guest test on each architecture reads and writes TLS variables of a `DT_NEEDED` library and of a `dlopen`ed one, from the main thread and from a thread created before the `dlopen`
- [ ] `dlopen`, `dlsym`, `dlclose`, `dladdr`
- [ ] `LD_PRELOAD` and `LD_LIBRARY_PATH`, useful for debugging more than for anything else; the loader ignores both for a set-user-ID program, which §13.9 marks with `AT_SECURE`, and an in-guest test, run as an ordinary user, checks that an `LD_PRELOAD` library never loads into a set-user-ID-root C test program linked against the §14.1 sysroot's `libc.so`, since `passwd` is a static §10.5-runtime binary with no interpreter to load one
- [ ] `libc.so` invoked as `ldd` lists each `DT_NEEDED` object and the path it resolved to

### 14.3 init and services
- [ ] PID 1: mount the base filesystems, start services, reap orphans, handle shutdown
- [ ] a declarative service definition: dependencies, restart policy, environment, working directory
- [ ] dependency-ordered parallel startup
- [ ] service supervision with restart backoff
- [ ] log collection from service stdout and stderr into the system log
- [ ] socket activation, which is genuinely elegant and not much work once sockets exist
- [ ] shutdown: signal services, wait with a timeout, unmount, and power off through `reboot` (ACPI on x86_64, PSCI on aarch64)
- [ ] a control tool for start, stop, restart, status, and logs
- [ ] before `login` lands, DESIGN §2.10's interim security posture goes back to the owner, since its acceptance (design review G006) assumed one user: an agent writes an OWNER DECISION block in §2.10 that states what this phase adds (logins, password hashes, a second user sharing the machine) and the options, and `login` merges only after §2.10 records the answer, together with any §18.3 box the answer moves ahead of it
- [ ] `login` and a getty on `/dev/tty1` and on the serial TTY (`/dev/ttyS0` or `/dev/ttyAMA0`, §13.7); `passwd` and `su`, installed set-user-ID root; a shadow file hashed with §14.7's password hash, readable only by root. `login` and `su` switch identity with §13.9's calls, which §13.9 checks, and `passwd` changes only the invoking user's entry unless run by root
- [ ] the DESIGN §8.3 e2e contract survives login. `make rootfs` with a test overlay builds a harness root image, which no release image or §14.6 release manifest contains. The overlay is also the only way a test trust anchor reaches a guest. Test CAs, test signing keys and key sets, a test update channel's key, and the keys the harness logs in with are generated per run where the test allows, as §18.7's Secure Boot test generates its keys, and otherwise live in `tests/keys/`, whose README lists each one's fingerprint and the lines that use it; the private keys there are public, so an image that trusts one trusts anyone. An anchor enters only the image this overlay builds, files the harness adds to an installed system before the test that needs them, or a service the harness runs on the host, never a §14.6 recipe or a release artifact, and §14.6's test-anchor check refuses a release that carries one. The overlay adds a test user and runs the getty on `/dev/tty1` and on the serial TTY with `--autologin <user>`, as agetty does, and that user's profile prints the registered `shell ready` marker when its TTY is the serial one. The marker order, the §13.7 serial and `sendkey` echo checks, and every later line that waits for `shell ready` run unchanged in that image. The Phase 14 `login` gate line boots the overlay with autologin off and types the test user's credentials over serial. DESIGN §8.3 updated in the same commit

### 14.4 Coreutils
- [ ] file and directory: `ls`, `cp`, `mv`, `rm`, `mkdir`, `rmdir`, `ln`, `touch`, `stat`, `find`, `du`, `df`
- [ ] text: `cat`, `head`, `tail`, `wc`, `sort`, `uniq`, `cut`, `tr`, `grep`, `sed`, `diff`
- [ ] process: `ps`, `kill`, `top`, `time`, `nice`
- [ ] system: `uname`, `date`, `uptime`, `free`, `mount`, `dmesg`, `env`, `id`, `hostname`
- [ ] archive: `tar`, `gzip`
- [ ] editor: something small, `vi`-flavored. Editing on the machine matters more than it sounds like.
- [ ] each with real argument parsing and correct exit statuses, because scripts depend on both

### 14.5 Shell
- [ ] a POSIX-shaped shell: word splitting, quoting, expansion, globbing
- [ ] redirection including here-documents and file descriptor manipulation
- [ ] pipelines, `&&`, `||`, `;`, subshells, command substitution
- [ ] variables, environment, `export`, arithmetic expansion
- [ ] control flow: `if`, `while`, `for`, `case`, functions
- [ ] job control: background, `fg`, `bg`, `jobs`, `wait`
- [ ] interactive: history, completion, line editing, prompt expansion
- [ ] runs as a `#!` interpreter and as `sh <file>`, with enough correctness to run a build script (§13.10 execs `#!` files)

### 14.6 Packaging
- [ ] a package format: metadata, dependencies, file list, checksums, install scripts
- [ ] a local package database with installed files and owners
- [ ] install, remove, upgrade, query, verify, with file conflict detection
- [ ] dependency resolution, and a clear error rather than a partial install
- [ ] a build recipe format and a tool that produces packages reproducibly
- [ ] a repository format signed with §14.7's Ed25519, read from a local directory or a mounted image; §15.8 fetches it over HTTP
- [ ] signature verification on every install and upgrade, with an unsigned or tampered package refused
- [ ] the base system itself shipped as packages, which is the test that the format is real
- [ ] a signed release manifest: every artifact of a tagged release with its SHA-256, as lines `sha256sum -c` reads, signed with an Ed25519 release key in the next box's signature format and published by the release workflow from `v0.14.0` on. Where the private key lives is the key-custody record below; its public key reaches the tree and every image only through the key-set record of the next box. §18.7's boot-chain manifest and §22.1 extend this format rather than adding another
- [ ] signing keys that can be replaced: every signature (package, repository index, release manifest) names its key by a key id; every signature the release and root keys make is an SSHSIG, the format OpenSSH's `PROTOCOL.sshsig` documents, over Ed25519, with one namespace per kind of signed object (`vibeos-package`, `vibeos-repo`, `vibeos-manifest`, `vibeos-keyset`), so stock `ssh-keygen -Y sign` makes it and `ssh-keygen -Y verify` checks it; a key id is the SHA-256 fingerprint of the key's OpenSSH public key; the §14.7 crate verifies the format, with host tests against `ssh-keygen` output; an image trusts a key set, not a single key; each release and each repository publishes a signed key-set record listing the keys to trust, the keys revoked, and a version that only increases, so an installed system moves to a new key, or drops a leaked one, through an ordinary update, and refuses a record whose version is older than the one it holds. A signature by a revoked key is refused. The record also carries the root set: the root keys and a threshold. It is signed by the threshold of the root set the image already holds, keys that sign nothing else, so a leaked release key is revoked by a record the thief cannot sign. A record that changes the root set is signed by the threshold of both the old set and the new one, as TUF rotates its root, and an image more than one root change behind applies each intermediate record in order, so every release and repository publishes every key-set record since the first. The first root set, in `v0.14.0`'s images, is the owner's one root key with threshold 1; adding a backup root key, which lets a lost or leaked root key be replaced without a reinstall, is one such record, and whether to hold one, and where, is the owner's call under the key-custody record below. Host tests rotate the release key twice and revoke it once, refuse a rolled-back record, and change the root set twice (add a second key, then remove the first), and an image built with the first root set follows both changes; an in-guest update across a rotation installs, and a package signed by the revoked key is refused. Without this, the first leaked key stays trusted by every image already installed, and the §39.1 freeze would make that permanent
- [ ] signed metadata that cannot be replayed or frozen: the repository index carries a `version` that rises with every signing, the release manifest carries its release's version, and each carries an `expires` time 120 days after it is signed. The package manager keeps the highest index version it has accepted for each repository and refuses an older one, and the §22.2 updater refuses a manifest for a release older than the installed one. For an automatic update, expired metadata is refused with a named error, and the update tool reports `metadata expired, no newer release seen`; root may accept it with an explicit flag, for an offline mirror. Expiry does not apply to the harness's release check below, to a manual install, or to the §39.2 corpora. From `v0.14.0` on, the release workflow signs fresh metadata at every release, and when 90 days pass without a signing, a scheduled run of it re-signs the current index and each supported branch's newest manifest with a later expiry. That run comes from a `schedule` trigger `release.yml` gains here, which GitHub runs only from `main`'s workflow file, so §10.1's rule that the release workflow is always `main`'s holds; its `sign` job signs under the key-job rule of the custody box and waits for the owner's approval as a release run does, one more step for the owner under the key-custody record below. Host tests replay an older index and an older manifest and serve an expired index; each is refused, and the expired one is accepted only with the flag

- [ ] key custody as the record below sets it: a `release` GitHub environment whose only required reviewer is the owner's own account holds the release key as its only secret, and only `release.yml`'s `sign` job names that environment, which admits deployments from `main` alone; that job follows the key-job rule: a job that holds a signing key or a write token runs no code from the candidate commit, restores no cache, checks out nothing, and receives only the `build` job's artifacts and their SHA-256 list, which it checks with `sha256sum -c` before it signs with the runner image's `ssh-keygen -Y sign` and, from §18.7, `sbsign` from Ubuntu's `sbsigntool` package, installed in that job; no key enters a guest, a cache, or a job that runs candidate code; the root key is made and used where DESIGN §2.10's agent-boundary answer says (recommended there: macOS's SIP-protected `/usr/bin/ssh-keygen`, `-t ed25519` and then `-Y sign -n vibeos-keyset`, run in a second macOS account that holds no agent tooling or credentials, with the key written passphrase-encrypted to removable media and used only while no agent session runs), kept offline, and used only to sign key-set records; the hostlib `release-manifest` tool builds and verifies key-set records and never signs; a repository ruleset lets only the owner's own account create `v*` and `phase-*` tags, with no bypass actor; `docs/RELEASING.md` lists the owner's steps (set up the account agents use and move the owner's own GitHub credentials off the account agents run under, as the agent-boundary answer sets it; create the root key; create the environment, its secret, and the tag ruleset; approve a release run or a metadata-only re-sign run; rotate or revoke a key; and, if the owner chooses, add a backup root key), and `scripts/check_workflows.py` (§10.1) fails when a workflow other than `release.yml` names the `release` environment. This box waits for the owner's answer to DESIGN §2.10's agent-boundary block: no key is created before it is recorded, and a procedure the owner chooses instead replaces the recommended one here

**Key custody (owner decision, 2026-09-23, design review H007): option (b).** The root key, which signs
only key-set records, stays offline with the owner and never enters CI. The release key signs each
release's manifest, the packages, and the repository index. It is a secret of a GitHub environment
that only the release workflow uses, and only after the owner approves the run. §18.7's Secure Boot db
key and §22.4's on-vibeOS signing follow the same rule. Why: a CI compromise can then sign only until
the owner revokes the release key, and the revocation reaches installed systems with their next
update. The owner's cost is creating the root key once, signing a key-set record at each rotation, and
approving each release run, which the owner starts anyway. Rejected: (a) both keys as CI secrets, where
one compromise could sign a key-set record that makes every image trust the attacker's key; and (c)
keyless Sigstore signing, which ties offline installs to a third party's trust root. The owner chose
(b) "for now". No key exists before `v0.14.0`, and changing custody is the owner's decision, not an
agent's.

**Where the keys are used (design review J022, 2026-09-24).** The custody above is unchanged. The
release key and §18.7's db key are used only in `release.yml`'s `sign` job on a hosted runner, after
the owner approves the run, under the key-job rule of the custody box; neither enters a vibeOS guest or
a job that runs candidate code, so §22.4 builds and verifies a release on vibeOS and signs it on the
host. Signing in the guest would have left the key in the memory of an unreleased kernel, beside the
candidate's whole build, where §10.7 publishes a failed run's core.

- [ ] the harness verifies a downloaded release ISO against the manifest before booting it, and refuses a tampered one; the check runs a hostlib `release-manifest` tool built from the §14.7 crate, as the harness runs `mkfs-vibefs`, so the harness stays standard library only (§0.6); the tool builds manifests and verifies them but never signs, since `release.yml`'s `sign` job uses stock `ssh-keygen` (the custody box)
- [ ] the release workflow refuses to publish a release that carries a test trust anchor (§14.3). In a job that holds no signing key and no write token, a check built on the hostlib tools that read vibefs images and §14.6 packages unpacks every image and package payload of the release and fails, naming the file, on: a public key, fingerprint, or certificate from `tests/keys/`, in any encoding, anywhere in the bytes; a trust store holding what the release did not put there: a key-set record other than the release's own, any `authorized_keys` file, and each store a later line adds to this check; and a package whose signature names a key id outside the release's key-set record. A host test plants each kind in a copy of a small release and expects a refusal that names it

### 14.7 Crypto primitives
Moved here from Networking, which had them from Hardening: login (§14.3) and signed packages (§14.6)
need them in this phase, and none of them needs the network. X.509 and TLS stay in §15.11. Disk
encryption and boot integrity stay in §18.7.

- [ ] one crypto crate, `vibeos-crypto`, built for the user runtime, the kernel, and hostlib's tools, so login, packaging, the release manifest, TLS, `sshd`, the CSPRNG, and §18.7's disk encryption share one implementation. It is a facade over pinned, permissively licensed crates that build `no_std`, not primitives written in-tree: RustCrypto's `sha2`, `sha3`, `hmac`, `hkdf`, `aes`, `aes-gcm`, `chacha20`, `chacha20poly1305`, `p256` and `p384` (ECDSA with RFC 6979 nonces, and ECDH), `rsa` (verification only), `argon2`, `sha-crypt`, and `bcrypt` (the last two verify the crypt(3) hashes musl writes), and dalek's `ed25519-dalek` and `x25519-dalek`. Each enters through §10.9's `deny.toml` allow list with the PR note AGENTS.md requires, and `deny.toml` bans `ring`, `aws-lc-rs`, `aws-lc-sys`, and `openssl-sys`, so no C or assembly crypto arrives through a default feature. In-tree crypto is limited to the entropy pool, the CSPRNG's construction over the crate's ChaCha20, the manifest, key-set, and package signature formats, and glue; host tools generate keys from the host OS's CSPRNG. Every signature on a package, repository index, release manifest, or key-set record is checked with `ed25519-dalek`'s `verify_strict`, which refuses a non-canonical `S` and a small-order key or `R`, and that check is part of the formats §39.1 freezes. The facade exposes no RSA private-key operation, since `rsa` has an open timing advisory against them (RUSTSEC-2023-0071), and `deny.toml` ignores that advisory with that reason. A crate that cannot build for a kernel target, or fails the license allow list, is replaced by an in-tree primitive with a Wycheproof gate, and this box records which and why
- [ ] an entropy pool: `RDRAND` and `RNDR`, virtio-rng, timer jitter, interrupt timing, with health checks (`/dev/random` already prefers virtio-rng then `RDRAND`; S1)
- [ ] `RDRAND` is self-tested once at init: it takes 8 samples and is disabled unless at least 5 differ from the sample before, as Linux's `arch/x86/kernel/cpu/rdrand.c` does, and its CPUID bit is cached instead of read on every call; no source's output reaches a reader without passing through the pool (F140)
- [ ] a CSPRNG behind `/dev/random`, `/dev/urandom`, and `getrandom`; `getrandom` and `/dev/urandom` never block after the pool is seeded, and `/dev/random` blocks only until then
- [ ] hashes and ciphers: SHA-2, SHA-3, AES-GCM, ChaCha20-Poly1305
- [ ] public key: Ed25519, X25519, RSA verification
- [ ] NIST P-256 and P-384: ECDSA signing and verification, and ECDH for TLS key exchange, since a large share of web certificate chains (ISRG Root X2 and Let's Encrypt's E-series intermediates among them) and many SSH host keys are ECDSA (§15.11, §15.8); host-tested against the Wycheproof vectors as well as the RFC ones
- [ ] a memory-hard password hash (Argon2id) for the shadow file
- [ ] the RFC and Wycheproof vectors as host tests of the facade, including Wycheproof's Ed25519 malleability and small-order cases, which `verify_strict` refuses; the same vectors run in-guest from `/bin/tests` on both architectures, which is what the gate checks

### 14.8 Root filesystem
vibefs v1 holds at most 4 MiB, 64 inodes, and 96 directory entries per volume ([VIBEFS.md](VIBEFS.md)
§3). The base system and the Phase 17 toolchains need a real one.

- [ ] vibefs format version 2, designed for the §12.5 page cache: multi-GiB volumes, at least a million inodes, B-tree directories without a volume-wide entry cap, and extent trees rather than four extents per inode, which `write` fills with multi-block extents; VIBEFS.md gains the v2 format before any code, as §8.5 did for v1, and it meets every requirement of VIBEFS.md §15: host tests of the shared format code write and read back a block number above 2^32, an inode number above 2^32, a 255-byte name, and a nanosecond timestamp, check CRC-32C against its published vectors, read one 4 KiB block of a 1 GiB extent with exactly one data-block read, overwrite one block in the middle of a 16-block extent and find the other 15 blocks' checksums unchanged, and refuse a metadata block whose header names another volume, another block number, or an unknown incompat feature (F051)
- [ ] a v2 directory lookup descends the on-disk B-tree, reading at most one block per tree level, instead of scanning the volume's flat 96-entry table as v1's `find_dent` does; a host test counts block reads for a lookup in a 10,000-entry directory (F067)
- [ ] a v2 commit makes every allocation before its superblock write (VIBEFS.md §10): a host test fails the commit's Nth heap or block allocation for every N the commit makes and finds disk and memory at the old generation each time, and a commit whose allocator fails every call once the superblock write starts finishes at the new generation
- [ ] the kernel, `mkfs-vibefs`, and `fsck-vibefs` share the v2 code, as they do for v1, and v1 is retired once root is v2: the kernel's v1 code is deleted and nothing upgrades a v1 volume in place, because no v1 volume holds data anyone kept (every v1 image is a CI artifact or the RAM-backed `/vibe`) and each on-disk parser that `mount` reaches is attack surface (DESIGN §2.10); `mkfs-vibefs` and `fsck-vibefs` keep v1 only while a CI job still builds a v1 image, and the §12.5 and §13.9 crash-state workloads move to v2 in the same PR
- [ ] extended attributes in the v2 format and in tmpfs: the `getxattr`, `setxattr`, `listxattr`, and `removexattr` families with their `l` and `f` forms, in the `user.` and `trusted.` namespaces, and `EOPNOTSUPP` for `security.` (until §18.6 adds `security.capability`) and `system.`; the format stores any name, as VIBEFS.md §15 requires, so later namespaces need no format change, and `docs/LINUX.md` lists POSIX ACLs (`system.posix_acl_*`) as a deliberate gap until §36.1 lands them
- [ ] the §8.5 crash-consistency test runs on a v2 volume larger than any v1 limit, and §12.5's crash-state enumerator and volatile-cache block device run over v2's commit protocol
- [ ] `fsync` cost measured when v2 lands, since every `fsync` on v2 is a full copy-on-write commit with two flushes: `fsync` latency (p50 and p99) after a 4 KiB write, and a one-hour workload of small transactions that each end in `fsync`, on a vibefs v2 volume and on ext4 under Alpine's `linux-virt` (added to the §14.9 pin list) booted in the same guest shape on the same runner and virtio-blk device, under KVM on the §10.1 KVM leg; the numbers go in `docs/` and feed §25.7's decision
- [ ] root is vibefs v2 on a virtio-blk disk that `make rootfs` formats and populates on the host from the §14.6 packages, on both architectures; `root=` on the §10.2 command line names it, and the initrd, which keeps only what init needs to mount it, is the root when `root=` is absent

### 14.9 Linux userland
Unmodified Linux binaries, dynamically linked against musl, from Alpine. §17.7 takes its toolchains from
here, and Phases 15 and 16 their test tools. No binary is patched to run; a failure is a kernel bug or a
line in this file. glibc and Debian userlands are Phase 23.

- [ ] `chroot`, and a `vibeos-linux` helper that enters a Linux root with `/proc`, `/dev`, `/dev/pts`, `/dev/shm`, and `/tmp` mounted; `/sys` in Linux's layout is Phase 23
- [ ] `mkfs-vibefs --from <tarball>` builds a vibefs v2 image from a root filesystem archive on the host, taking owners, modes, device nodes, symlinks, and hard links from the archive headers, so no host needs root or a case-sensitive filesystem to build it
- [ ] Alpine's `minirootfs` for each architecture, pinned by release and SHA-256, and a package snapshot, a pinned package set with its `APKINDEX` on a disk image that `apk` reads as a repository; `apk` in the guest verifies the index signatures against the minirootfs keys. The set starts with what this phase's gate installs, and each later phase adds the packages its lines name
- [ ] the snapshot's packages fetched by SHA-256 when the image is built and cached in CI, never committed; Alpine keeps only the latest build of each package, so a pin that stops resolving fails the fetch with the package named, and moving the pin is a commit
- [ ] `apk` runs only inside a `vibeos-linux` root and never owns a file of the base system, which stays §14.6 packages
- [ ] §13.9's and §13.10's per-pid `procfs` files in Linux's exact formats, plus the system-wide files Alpine's tools read: `cpuinfo`, `meminfo`, `stat`, `uptime`, `loadavg`, `mounts`, `filesystems`, `version`, `sys/kernel/{osrelease,ostype,hostname,pid_max,random/boot_id}`, and `sys/vm/overcommit_memory`; each format host-tested against output captured from a Linux guest
- [ ] `/dev/fd`, `/dev/stdin`, `/dev/stdout`, and `/dev/stderr` as the symlinks Linux provides
- [ ] procps-ng's `ps`, `top`, and `free` from the snapshot report the same numbers as the native tools
- [ ] the §13.11 differential runner extended to this dynamic corpus
- [ ] `docs/LINUX.md` covers the dynamic corpus: what runs, what does not, and why

### 14.10 Ported sources
Rust crates are covered by §10.9's `cargo deny` policy and Limine by its pinned commit in `setup.sh`. Every
other third-party source the tree builds arrives this way, musl first. Sources and binaries used only by
tests and never shipped (packetdrill, the §14.9 mirror) are pinned by hash but need no source offer;
§13.11's corpus is published with its source archives beside it.

- [ ] `ports/<name>/port.toml` for every third-party source: upstream URL, version, SHA-256 of the archive, SPDX license identifier, and a numbered patch series beside it; the §14.6 recipe builds from it
- [ ] source archives mirrored as release assets, so a build does not depend on an upstream host staying up, and so every release carries the corresponding source of each copyleft package it ships, as assets of the same GitHub Release: the archive its `port.toml` pins by SHA-256, its patch series, and its recipe; the release job refuses to publish a release that ships a copyleft package without them
- [ ] a license policy in `docs/` that states the owner's decision recorded below, before §14.9 and before any copyleft binary ships in an image: vibeOS code is MIT, ports keep their licenses, a copyleft port ships as its own package with its corresponding source beside it, and no proprietary binary ships

**License policy (owner decision, 2026-09-23, design review H015): option (b).** Release images,
packages, and every published asset may carry permissive software (MIT, BSD, ISC, zlib, Apache-2.0,
and the like) and copyleft software (GPL, LGPL, MPL), and no proprietary binary. Device firmware
(§31.6) and CPU microcode (§20.1) are fetched by tests only and never published. The kernel stays MIT
(DESIGN §1.5), and a GPL program shipped beside it is aggregation, not a combined work.

How the copyleft source is provided: each release, and each other published asset that holds copyleft
binaries (§13.11's corpus, §17.7's build image), carries their corresponding source as assets of the
same GitHub Release. That is the exact upstream archive whose SHA-256 the port pins, the patch series,
and the build recipe, which the build already fetches and the release job uploads with no human step.
Offering the source from the same place as the binary satisfies GPLv2 §3, GPLv3 §6(d), the LGPL, and
MPL-2.0 §3.2 together, and costs nothing on a public repository.

Rejected: linking to each upstream project's download instead. GPLv3 allows a link only while the
distributor itself keeps it working, and GPLv2 does not clearly allow one at all. Alpine and many
upstreams delete superseded versions, so a link would quietly stop meeting the license. Rejected:
copying the source into the tree or into comments. DESIGN §1.5 forbids copying GPL code into the MIT
tree, and a copy there proves nothing that the archive beside the binary does not.

Publishing proprietary firmware or microcode, when a funded goal brings physical hardware, goes back
to the owner.

- [ ] `make check` fails when a port lacks a manifest or has a license the policy does not allow; a scheduled job verifies each archive hash and that each patch series applies
- [ ] the §14.9 mirror's pin list records each package's license from its own metadata, and the scheduled job checks it against the same policy
- [ ] a scheduled job looks up each port and each §14.9 mirror package in the OSV database and opens an issue for a known vulnerability

---

## Phase 15: Networking

**Goal.** A TCP/IP stack good enough to serve requests and fetch a package repository.

**Unlocks.** Remote access, package distribution, and the largest available source of well-specified
protocol work for an agent to get wrong in interesting ways.

**Architectures.** Both. virtio-net and everything above the netdev layer are shared. e1000 is PCI,
builds for both, and its in-guest tests run under QEMU (`-device e1000`) on both; Phase 20 runs the
drivers for other real NICs on QEMU's models of them. Strict packetdrill timing is x86_64 only: the arm64 runners have no KVM and the macOS dev host no tap device for its wire server, so aarch64 runs it under TCG with a looser tolerance (§15.10). The §10.1 KVM leg measures the throughput
numbers on x86_64. The aarch64 numbers are measured under HVF on the dev host, since GitHub's arm64
runners have no KVM.

**Exit gate**
- [ ] `ping` from the host to the guest and back
- [ ] DHCP acquires an address, and DNS resolves a name
- [ ] a TCP server in the guest serves a file to `curl` on the host, and the bytes match
- [ ] a TCP client fetches 1 GiB from the host with no corruption, at 1 Gbit/s or better over virtio-net and 4 Gbit/s or better over loopback, both under KVM in a 2-vCPU, 512 MiB guest
- [ ] the §15.10 packet fuzzer, injecting malformed headers, bad checksums, and overlapping fragments through the injection interface, runs one hour per architecture on the nightly job in a 2-CPU, 512 MiB guest (KVM on x86_64, TCG on aarch64) with no panic or hang
- [ ] a ten-minute flood from a hostlib peer on the §15.10 tap device, per architecture on the nightly job in a 2-CPU, 512 MiB guest (KVM on x86_64, TCG on aarch64), of out-of-order TCP segments over many connections to a listening service and of SYNs from unresolvable addresses on the guest's subnet, holds socket memory at or below §15.1's global limit with the drop counters rising, kills no process, and leaves `meminfo`'s progress-class low-water mark above zero, while a file a guest process writes on `vda` is `fsync`ed and reads back intact (§15.10)
- [ ] `netstat`-equivalent shows sockets in correct states through a full connection lifecycle
- [ ] an HTTPS fetch from a server on the host completes with the certificate chain verified against a test CA, and a tampered certificate is refused, using §15.11
- [ ] an `sshd` login from the host reaches a shell on a pty
- [ ] SNTP against the §15.10 host responder sets the wall clock to within 100 ms of the host
- [ ] host tests for header parsing, checksums, TCP state transitions, and sequence arithmetic, and a Kani proof of the sequence and window comparisons for every pair of 32-bit values (§15.10)
- [ ] the nightly sweep of 10,000 seeds of simulated 1 MiB TCP transfers over a lossy, duplicating, reordering, corrupting link finishes with no corruption, hang, or panic, and every past failing seed is a checked-in host test (§15.10)
- [ ] packetdrill in wire mode runs every upstream TCP script that does not assert a Linux-only socket option, `TCP_INFO` field, or sysctl setting on the nightly job, in a 2-CPU, 512 MiB guest under KVM on x86_64 and TCG on aarch64, and each one passes or is on §15.10's checked-in list with the RFC section that allows the difference; under TCG, with Linux's slow-machine tolerance, a script that fails only with packetdrill's `timing error` is instead an expected failure in the job summary (§15.10)
- [ ] on both architectures, unmodified tools from the §14.9 mirror work against virtio-net: iproute2's `ip addr` and `ip route` list and change the interface's address and routes, busybox `udhcpc` acquires a lease, and `tcpdump` with a `tcp port 80` filter captures only that traffic
- [ ] tag `phase-15` and cut the next release

### 15.1 netdev layer
- [ ] a `NetDevice` trait: transmit, MTU, MAC, link state, and statistics
- [ ] receive queues delivering into the stack from each queue's threaded bottom half, polled up to a budget per wake, and loopback from a softirq-equivalent item on the sending CPU (DESIGN §2.2), never from the hard IRQ; every receive allocation is in DESIGN §4.4's atomic class and is charged to a socket or to a bounded queue, and the per-CPU backlog that loopback and any deferred receive use is bounded as Linux's `netdev_max_backlog` bounds it, dropping and counting past it
- [ ] socket memory accounted as Linux accounts it: a socket's queued receive data, its TCP out-of-order queue included, is bounded by `SO_RCVBUF`, and its unsent and unacknowledged data by `SO_SNDBUF`, with Linux's defaults and maxima (`net.core.rmem_default`, `rmem_max`, `wmem_default`, and `wmem_max`, and TCP's `tcp_rmem` and `tcp_wmem`), set and read back through §15.7's `setsockopt` and `getsockopt` as Linux does, which doubles the value set; all sockets together are bounded by a global limit sized from RAM in the shape of Linux's `tcp_mem` and `udp_mem`; past a socket's bound or the global limit a segment or datagram is dropped and counted, and nothing is reclaimed for it (DESIGN §4.4). A host test fills one socket's receive buffer with out-of-order segments and finds the next one dropped and counted
- [ ] a packet buffer type with headroom and tailroom so headers can be prepended without copying
- [ ] checksum and segmentation offload flags, used when the device supports them
- [ ] a loopback device, which is also the easiest way to test everything above it
- [ ] per-device statistics: packets, bytes, errors, drops
- [ ] everything above the netdev layer takes time, timers, and randomness through one trait: the kernel implementation uses the monotonic clock and the §14.7 CSPRNG, and the host build drives the stack with a simulated clock and a seeded generator (§15.10)

### 15.2 Drivers
- [ ] virtio-net: receive and transmit virtqueues, mergeable receive buffers, checksum offload, multi-queue
- [ ] e1000, because QEMU's `e1000` models a real Intel 82540EM and the chip is well documented
- [ ] a threaded handler on a level-triggered route (I/O APIC INTx, or a GIC SPI from the device tree's `interrupt-map`) has its line masked in `irq_init::dispatch` before the EOI and unmasked after the bottom half returns, and `set_threaded` refuses `top: None` on a level route without that masking; in-guest, e1000, whose QEMU model has no MSI, takes a receive flood at `-smp 2` through that path while the other CPU keeps scheduling (F099)
- [ ] in-guest driver tests against a loopback QEMU network configuration

### 15.3 Link layer
- [ ] Ethernet framing, parsing, and dispatch by EtherType
- [ ] ARP with a cache, timeouts, request queueing for unresolved destinations, and gratuitous ARP handling; each unresolved neighbour's queue is bounded in bytes as Linux's `unres_qlen_bytes` bounds it, dropping and counting its oldest packet past the bound, and the neighbour table is capped as Linux's `gc_thresh3` caps it
- [ ] VLAN tagging, cheap to add and annoying to retrofit
- [ ] neighbor discovery for IPv6 later, with ARP structured so it is not a special case

### 15.4 IP
- [ ] IPv4 header parse, validate, and construct, with checksum
- [ ] routing table with longest-prefix match, a default route, and per-route MTU. A connected socket caches a counted reference to its route with the table's generation number; every route, address, link-state, or per-destination MTU change bumps the generation, and a transmit that finds its generation stale looks the route up again, as Linux's route cookie check does. The next hop's neighbour entry is looked up on each transmit, not cached in the socket. A deleted route is unpublished, the generation bumped, and the entry freed at its last put (DESIGN §2.11 rule 3). In-guest: after `ip route replace` lowers a connected TCP socket's route MTU to 1280, the next segment it sends, captured through `AF_PACKET`, fits in 1280 bytes, and after close the route-entry count is back at its baseline
- [ ] fragmentation and reassembly, with a reassembly timeout and a bound on held fragments so it is not a memory attack
- [ ] ICMP: echo, destination unreachable, time exceeded, and correct generation on error
- [ ] TTL handling and forwarding, which makes it a router with very little extra work
- [ ] the header code in the library half, host-tested against captured packets

### 15.5 UDP
- [ ] datagram send and receive with port binding and demultiplexing
- [ ] checksum computation and validation, including the optional-zero case
- [ ] receive queue per socket bounded by its `SO_RCVBUF` under §15.1's accounting, with a drop counter
- [ ] the socket backlog of DESIGN §2.1, which lands here because UDP is the first receive path that cannot sleep: a datagram for a socket that a syscall owns goes on its bounded backlog, the owner drains it as it releases the owner lock, and a full backlog drops the datagram and counts it; TCP and §15.7's ICMP and packet sockets reuse it
- [ ] connected UDP sockets
- [ ] enough to run DNS and DHCP, which is what unblocks everything else

### 15.6 TCP
- [ ] the full state machine, transitions tested exhaustively as a host test
- [ ] three-way handshake, and connection teardown including simultaneous close and `TIME_WAIT`
- [ ] sequence and acknowledgement arithmetic with wraparound handled, host-tested
- [ ] send and receive buffers with a sliding window and zero-window handling, bounded by `SO_SNDBUF` and `SO_RCVBUF` under §15.1's accounting, with the out-of-order queue counted against the receive buffer, as Linux counts it
- [ ] retransmission with an RTO from an RTT estimator, exponential backoff
- [ ] TCP's retransmit, delayed-ACK, zero-window-probe, keepalive, and `TIME_WAIT` timers are DESIGN §2.2 timer callbacks: each takes the socket's spinlock and defers to the owner through the backlog when a syscall owns the socket, holds a counted reference to the socket while pending, and is cancelled with `cancel_sync` before the socket is freed; TCP input runs under the socket's spinlock and allocates fallibly there
- [ ] fast retransmit and fast recovery
- [ ] congestion control: slow start, congestion avoidance, and a modern algorithm afterward
- [ ] delayed ACK, Nagle, and a `TCP_NODELAY` option, since interactive traffic and bulk traffic want opposite things
- [ ] window scaling, timestamps, and selective acknowledgement
- [ ] keepalive, and a listen backlog with SYN flood resistance
- [ ] path MTU discovery
- [ ] this is the single largest correctness surface in the project; the test suite matters more than the implementation

### 15.7 Socket API
- [ ] the §13.3 socket calls (`socket`, `bind`, `listen`, `accept`, `accept4`, `connect`, `sendto`, `recvfrom`, `shutdown`) extended to `AF_INET`; §15.9 adds `AF_INET6`
- [ ] `getsockopt` and `setsockopt` for the options that exist, and a clear error for those that do not
- [ ] `getsockname`, `getpeername`
- [ ] non-blocking mode integrated with `ppoll` and `epoll`
- [ ] `sendmsg` and `recvmsg` (from §13.3) for `AF_INET`, with scatter-gather and IP-level control messages
- [ ] `SO_REUSEADDR`, `SO_REUSEPORT`, `SO_BROADCAST`, `SO_BINDTODEVICE`, and sending from `0.0.0.0` on an interface with no address yet, which a DHCP client needs before it has a lease
- [ ] ICMP sockets for `ping` and `traceroute`: `SOCK_DGRAM`/`IPPROTO_ICMP` ping sockets and `SOCK_RAW` ICMP sockets; `IP_TTL` on UDP and ICMP sockets; ICMP errors delivered to the socket that caused them
- [ ] Linux's `AF_PACKET`: `SOCK_RAW` and `SOCK_DGRAM` bound to one interface with `sockaddr_ll`, `PACKET_ADD_MEMBERSHIP` for promiscuous mode, `PACKET_AUXDATA`, and `PACKET_RX_RING` with `TPACKET_V2` and `TPACKET_V3` rings mapped with `mmap`, which libpcap requires; the `tcpdump`-equivalent and busybox `udhcpc` use it
- [ ] classic BPF socket filters (`SO_ATTACH_FILTER`, `SO_DETACH_FILTER`, `SO_LOCK_FILTER`) on packet, UDP, and TCP sockets, run by one interpreter in the portable half that validates each program before attaching it; host-tested against filters compiled by libpcap and fuzzed like every parser; §18.6's `seccomp` reuses it
- [ ] Linux's `AF_NETLINK` `NETLINK_ROUTE`: `RTM_GETLINK`, `RTM_GETADDR`, and `RTM_GETROUTE` dumps with `NLM_F_MULTI`; `RTM_NEWLINK` for link up and down; `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_NEWROUTE`, and `RTM_DELROUTE`; and the `RTMGRP_*` multicast groups for change notifications, so musl's `getifaddrs` and `if_nameindex` and unmodified iproute2 work
- [ ] `SIOCGIFCONF`, `SIOCGIFFLAGS`, `SIOCGIFADDR`, `SIOCGIFHWADDR`, `SIOCGIFMTU`, and `SIOCGIFINDEX` through the §13.9 registry, for software that still calls them
- [ ] `/proc/net/tcp`, `/proc/net/udp`, and `/proc/net/unix` list every socket with its addresses and state, which the `netstat`-equivalent and `ss` read
- [ ] `sockaddr_in`, `sockaddr_in6`, `sockaddr_ll`, `sockaddr_nl`, `ifreq`, `sock_fprog`, `tpacket2_hdr`, `tpacket3_hdr`, `nlmsghdr`, `ifinfomsg`, `ifaddrmsg`, and `rtmsg` added to §13.10's struct table, and the `SOL_SOCKET`, `IPPROTO_IP`, `IPPROTO_IPV6`, and `IPPROTO_TCP` option numbers and `CMSG_ALIGN` checked against the uapi headers on both architectures

### 15.8 Configuration and tools
- [ ] a userspace DHCP client over §15.5 UDP broadcast that applies its lease through §15.7's netlink interface: discover, request, lease renewal, and correct behavior on lease expiry
- [ ] DNS resolver: A, AAAA, CNAME, with a cache honoring TTLs, retries, and multiple servers
- [ ] static configuration files and an `ip`-equivalent tool
- [ ] `ping`, `traceroute`, `netstat`, `ss`, `tcpdump`-equivalent
- [ ] an HTTP client good enough to fetch packages from a §14.6 repository, and a server good enough to prove the stack
- [ ] an SNTP client so `date` is right
- [ ] `sshd` on §14.7's primitives (SSH does its own key exchange; it does not use TLS), with §14.3's login and §13.7's ptys, and a login from a host client in CI; this is the point at which the machine becomes genuinely usable remotely

### 15.9 IPv6
- [ ] addressing, header parsing, extension headers
- [ ] neighbor discovery and stateless address autoconfiguration
- [ ] ICMPv6
- [ ] dual stack sockets
- [ ] deliberately after IPv4 works, and deliberately not skipped

### 15.10 Testing
- [ ] a packet injection interface so the stack can be tested without a real device
- [ ] replay of captured traffic as host tests
- [ ] `cargo-fuzz` targets for every parser this phase adds, from packet headers and options to netlink, DHCP, DNS, BPF programs, and X.509, on the weekly job as §10.2's are; each crash becomes a replayed regression
- [ ] an in-guest packet fuzzer: from one seed printed on serial it injects malformed headers, bad checksums, and overlapping fragments through the injection interface into the running stack, so a crash replays from its seed; the nightly job runs it one hour per architecture in a 2-CPU, 512 MiB guest (KVM on x86_64, TCG on aarch64), and each crash's seed is checked in as a replay
- [ ] the memory flood of the Phase 15 gate: a hostlib peer on the tap device sends out-of-order TCP segments over a set number of connections, and SYNs from a range of unresolvable addresses, at a set rate, while a guest process writes and `fsync`s a file on `vda`; the run reads socket memory and the §12.6 low-water marks from `meminfo`, and the per-socket, per-neighbour, and per-device drop counters, and prints them in the job summary
- [ ] throughput and latency benchmarks with regression thresholds, on the §10.1 KVM leg, recording also the share of received segments processed from a socket's backlog on its owner's release
- [ ] host-to-guest integration tests in CI over QEMU user networking and a tap device
- [ ] an SNTP responder on the host that the guest reaches from CI (QEMU user networking has no NTP service of its own), serving the host clock the Phase 15 SNTP gate measures against
- [ ] a deliberately hostile peer: reordering, duplication, loss, tiny windows
- [ ] deterministic simulation: instances of the portable stack on the host, over a simulated link that drops, duplicates, reorders, delays, and corrupts, under the §15.1 simulated clock, all driven from one seed
- [ ] a nightly sweep of seeds; a failing seed is printed, replays bit for bit as a host test, and is checked in
- [ ] the socket owner hand-off in the portable half with a loom model (§10.8): a receive appending to the backlog while the owner releases the owner lock, and a timer callback racing `close`'s `cancel_sync`; a weakened variant that clears the owned flag before it drains the backlog, and one whose `cancel_sync` returns while the callback runs, must each fail the model
- [ ] packetdrill, built statically in §13.11's digest-pinned Alpine container with `linux-headers` and pinned under §14.10, run in wire mode inside a §14.9 `vibeos-linux` root, since its client configures the interface by running iproute2's `ip`: the client executes each script's syscalls on vibeOS while its server on the host injects and checks packets on the tap device; the runner replaces upstream's `defaults.sh`, which writes Linux sysctls, with one for vibeOS
- [ ] the upstream packetdrill TCP scripts that do not assert Linux-only socket options, `TCP_INFO` fields, or sysctl settings run on the nightly job in a 2-CPU, 512 MiB guest, on x86_64 under KVM on the §10.1 KVM leg with packetdrill's default tolerance, and on aarch64 under TCG with `--tolerance_usecs=14000`, which Linux's kselftest runner passes on a slow machine; each passes or is on a checked-in list with the RFC section that allows the difference, and the list only shrinks. On the TCG run alone, a script that fails only with packetdrill's `timing error` is an expected failure written to the job summary, never a list entry
- [ ] sequence-space comparison and window arithmetic from §15.6 proved by Kani (§10.8) for every pair of 32-bit values

### 15.11 TLS
The primitives, the entropy pool, and the CSPRNG are §14.7. This is the part that needs a peer. Package
fetching over HTTPS (§15.8) needs it before Hardening; the upstream `cargo` and `git` that §17.7 runs bring their own. `sshd` does not, since SSH
brings its own key exchange.

- [ ] X.509 parsing and chain validation through `rustls-webpki` (ISC), with a bundled root store, host-tested against real certificates and fuzzed like every parser; the root store joins §14.6's test-anchor check, which refuses a root its pinned source does not list
- [ ] TLS 1.3 client and server in userspace through rustls (Apache-2.0, ISC, or MIT), with default features off and a `CryptoProvider` over the §14.7 facade, not a TLS state machine written in-tree; 1.2 only if a peer that matters demands it. The provider is security glue, so the nightly job runs rustls's BoGo shim with it against BoringSSL's test runner, with rustls's checked-in list of expected differences and a vibeOS list beside it that only shrinks. If rustls cannot build for the user runtime, this box records why and what replaces it
- [ ] an HTTPS fetch is the integration test, run in CI against a host peer with a test CA, which the guest trusts only through §14.3's harness overlay; a fetch from a public host is a manual check, not a gate

---

## Phase 16: Graphics and Windowing

**Goal.** More than one window. A compositor, an input stack, and a terminal emulator running in it.

**Unlocks.** The thing that makes people believe it is an operating system. Linux's DRM and evdev
interfaces, which later graphics and desktop software opens unmodified.

**Architectures.** Both. PS/2 is x86_64 only. virtio-input (§11.5) is the keyboard and pointer on
aarch64 and works on x86_64 too. virtio-gpu is shared. The frame-rate gate line is measured on the §10.1 KVM leg on x86_64; the aarch64 number is measured under HVF on the dev host and must meet the same threshold, since GitHub's arm64 runners have no KVM.

**Exit gate**
- [ ] multiple windows, movable and resizable: a scripted scene of overlapping windows raised, moved, and resized matches its §16.1 reference image after each step, which checks overlap and damage handling
- [ ] the terminal emulator runs the shell through a pty, and a scripted session exercising each group of §16.7's escape sequences matches its §16.1 reference images
- [ ] the harness injects key and pointer events with QMP `input-send-event` into two overlapping test clients that log the events they receive on serial; only the focused client logs the keys, and only the client under the pointer logs the motion, on both architectures
- [ ] unmodified `modetest` (libdrm) and `evtest` from the §14.9 mirror list connectors and set a mode with a dumb buffer on virtio-gpu, and read key and pointer events from virtio-input, on both architectures
- [ ] a screenshot captured programmatically and compared against a reference in CI (§16.1)
- [ ] a resolution change, and a second output added and removed, at runtime (§16.1): after each change a test client receives the new mode or the removal through §16.5 and commits its next buffer at the output's new size, and the §16.1 screenshot of each remaining output matches its reference
- [ ] the compositor holds 60 frames per second at 1920×1080 with ten windows moving, fewer than 1% of frames dropped over ten seconds, measured under KVM in a 4-vCPU, 2 GiB guest with virtio-gpu 2D
- [ ] the compositor killed with `SIGKILL` leaves the text console visible and taking keyboard input, on both architectures
- [ ] tag `phase-16` and cut the next release

### 16.1 Display abstraction
- [ ] a `Display` with a mode list, current mode, and framebuffer access
- [ ] the Limine framebuffer as the fallback, always available
- [ ] mode setting where the hardware supports it
- [ ] multiple outputs with positions, because a second monitor should not be a rewrite
- [ ] outputs added and removed at runtime: a new output gets a mode and a position, and surfaces on a removed output move to one that remains; exercised under QEMU with a two-head virtio-gpu (`max_outputs=2`): the harness sets the second head's size to zero and back through a VNC server on that head (`-vnc unix:<path>,display=<id>,head=1`), to which a hostlib client sends `SetDesktopSize`, since no QMP command resizes a head; it needs no D-Bus daemon, unlike QEMU's D-Bus display (`org.qemu.Display1.Console.SetUIInfo`), which carries the same request where a build has it. virtio-gpu raises its display-config event, and §31.7 gives every head a VNC server
- [ ] an output change reported as Linux reports it, a `change` uevent with `HOTPLUG=1` on a `NETLINK_KOBJECT_UEVENT` socket, after which clients re-read the connectors; the socket is in the `AF_NETLINK` family §15.7 also uses, built by whichever lands first
- [ ] vsync and page flipping, so tearing is fixable rather than inherent
- [ ] the text console double-buffered through the display abstraction, which closes §5.1's parked box
- [ ] a pixel format abstraction that is not hardcoded to BGRX
- [ ] the `Display` exposed to userspace as `/dev/dri/card<N>`, character device 226:<N>, which libdrm checks, with Linux's DRM uapi for the KMS subset it supports, through the §13.9 `ioctl` registry: `DRM_IOCTL_VERSION`, `GET_CAP`, `SET_CLIENT_CAP`, and the `DRM_IOCTL_MODE_*` calls for resources, connectors, encoders, CRTCs, planes, and properties; atomic commit (`MODE_ATOMIC`, with `TEST_ONLY`) with the property blobs its `MODE_ID` takes (`MODE_CREATEPROPBLOB`, `MODE_DESTROYPROPBLOB`), and legacy `SETCRTC` and `PAGE_FLIP` implemented over it, as Linux's helpers do. Ioctl numbers and struct layouts are host-tested against Linux's uapi headers on both architectures, and the subset, with the reason for each omission, is recorded in `docs/`
- [ ] dumb buffers (`MODE_CREATE_DUMB`, `MODE_MAP_DUMB`, `MODE_DESTROY_DUMB`), wrapped as framebuffers by `MODE_ADDFB2` and released by `MODE_RMFB`, that the compositor maps with `mmap` as a device mapping, not page-cache frames (cacheable on virtio-gpu, whose host reads the guest-RAM backing through a cacheable mapping, so a write-combining alias would go stale on aarch64 under KVM or HVF), and exports and imports as dma-buf file descriptors (`PRIME_HANDLE_TO_FD`, `PRIME_FD_TO_HANDLE`; the import is how §16.3's fullscreen bypass scans out a client's buffer); page-flip and vblank events read from the file descriptor, which wake `ppoll` and `epoll` (§13.6)
- [ ] one owner of the display at a time, as Linux's DRM master: the first process to open the primary node while it has no master becomes master, as `modetest` expects, and `SET_MASTER` and `DROP_MASTER` move it; while a process holds it, the kernel text console stops drawing to the scanout; when the holder drops master, closes the node, or exits, the kernel restores the console's mode and redraws it, so a crashed compositor leaves a usable console
- [ ] the harness captures the framebuffer with QEMU's `screendump` over the monitor and compares it against a checked-in reference image with a stated per-pixel tolerance; a mismatch writes a diff image and fails the test, on both architectures

### 16.2 GPU
- [ ] virtio-gpu 2D: resource creation, transfer, `set_scanout`, flush
- [ ] virtio-gpu cursor plane, which is worth it for the latency alone
- [ ] EDID for mode discovery
- [ ] the `Display` and buffer interfaces admit a 3D backend ([Phase 33](#phase-33-graphics-stack)) without changing clients: each buffer carries a type and a completion fence, and a stub second backend builds against the interface
- [ ] a software rasterizer good enough that 3D is optional: lines, rects, blits, alpha, text

### 16.3 Compositor
- [ ] a surface tree with position, size, stacking order, and transforms
- [ ] damage tracking so only changed regions are recomposited
- [ ] alpha blending and clipping
- [ ] double or triple buffering with a frame callback protocol so clients throttle to the display
- [ ] a hardware cursor when available, software otherwise
- [ ] fullscreen bypass, sending a client's buffer straight to scanout
- [ ] the frame loop must not depend on client responsiveness; a hung client freezing the desktop is the failure mode to design against

### 16.4 Input
- [ ] an input event abstraction: keyboard, pointer, and touch, with timestamps, in Linux's event types and codes (`input-event-codes.h`), so the evdev nodes below translate nothing
- [ ] PS/2 mouse on x86_64, and virtio-input keyboard and pointer on both architectures; USB HID arrives through §20.3 into this same abstraction
- [ ] input devices exposed to userspace as Linux evdev nodes, `/dev/input/event<N>` (character device 13:64+<N>): `struct input_event` records framed by `EV_SYN`, and `EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPHYS`, `EVIOCGUNIQ` (`ENOENT` for a device with none), `EVIOCGPROP`, `EVIOCGBIT`, `EVIOCGABS`, `EVIOCGKEY`, `EVIOCGLED`, `EVIOCGSW`, and `EVIOCGREP`, which libevdev's `libevdev_set_fd` issues, and `EVIOCSCLOCKID`, which libinput sets through `libevdev_set_clock_id`, with ioctl numbers and layouts host-tested against Linux's uapi headers; read with `read` and waited on with `ppoll`/`epoll`
- [ ] `EVIOCGRAB` as the exclusive grab: while it is held, the §13.7 console TTY stops consuming keyboard and pointer input, serial input is unaffected, and the grab drops when the holder closes the node or exits
- [ ] event routing: pointer to the surface under the cursor, keyboard to the focused surface
- [ ] focus policy, grabs, and click-to-focus
- [ ] keyboard layout handling with dead keys and compose, which is more work than it looks
- [ ] pointer acceleration, and scroll with kinetic behavior
- [ ] repeat rate and delay

### 16.5 Display protocol
Decided: the protocol is Wayland: its wire format, with the core, `xdg-shell`, `linux-dmabuf`,
`presentation-time`, `viewporter`, and `fractional-scale` protocols, served by the §16.3 compositor
and spoken by a client library in the user crate. A need no upstream protocol covers becomes an
extension in Wayland's XML format, documented in `docs/`. Why: Phase 36 runs unmodified Wayland
compositors and toolkits, so a native protocol would be retired there or kept as a second one beside
Wayland, and every client written in Phases 16 to 35 (the §16.7 terminal, the §16.6 toolkit) would
be rewritten. The work with no canonical answer, the compositor's surface tree, damage tracking, and
frame scheduling, and the toolkit, is unchanged. Rejected: a native protocol mapped onto Wayland
later, which this subsection planned before, and X11.

- [ ] protocol bindings generated from the pinned upstream XML (`wayland.xml` and `wayland-protocols`, both MIT-licensed, under §14.10), one generator for the compositor and the client library, so the two cannot disagree; `docs/` lists each protocol and version implemented
- [ ] the compositor socket (`$XDG_RUNTIME_DIR/wayland-0`) over §13.3's Unix sockets, with buffers shared through `wl_shm` over `memfd_create` (§13.4) and through `linux-dmabuf` from §16.1, and descriptors passed with `SCM_RIGHTS`
- [ ] surface lifecycle, commit semantics, and damage submission as `wl_surface` defines them
- [ ] outputs announced through `wl_output` with their position, mode, and integer scale, including their arrival and removal at runtime (§16.1); clients submit buffers at an output's scale, and `fractional-scale` carries a fractional one
- [ ] input event delivery through `wl_seat`'s keyboard, pointer, and touch
- [ ] window management through `xdg-shell`: title, app id, minimum and maximum size, state
- [ ] clipboard and drag and drop through `wl_data_device`
- [ ] unmodified `wayland-info` from the §14.9 mirror lists every global the compositor advertises, with the versions `docs/` records, on both architectures

### 16.6 Window management and toolkit
- [ ] window decorations, or a client-side decoration protocol
- [ ] move, resize, minimize, maximize, close
- [ ] tiling and floating layouts, workspaces
- [ ] a keyboard-driven window switcher
- [ ] a widget toolkit: layout, buttons, text entry, lists, scrolling
- [ ] font rendering with real glyph rasterization, hinting, kerning, and subpixel positioning, which is a project in itself
- [ ] a panel with a clock, a launcher, and a system tray

### 16.7 Terminal emulator
- [ ] a pty client with correct `termios` interaction
- [ ] VT100 and xterm escape sequence handling: cursor, colors, attributes, scroll regions, alternate screen
- [ ] 256-color and true color
- [ ] a scrollback buffer with selection and clipboard
- [ ] UTF-8, wide characters, and combining marks
- [ ] resize with `SIGWINCH`
- [ ] this is the single most important application on the system; it gets the same care as the kernel

---

## Phase 17: Self-hosting Toolchain

**Goal.** vibeOS compiles vibeOS. Check out the source on the machine, build it, boot the result. The
compilers are upstream Linux builds run unmodified (§17.7); rebuilding them from source is Phase 24.

**Unlocks.** The claim. Also a genuinely brutal test of the POSIX surface, since compilers exercise
everything.

**Architectures.** Both, and each cross-builds the other. The byte-identical comparison of cross and
native builds is in [Beyond](#beyond). Every gate line runs in a 4-vCPU, 4 GiB guest under KVM (the §10.1
KVM leg on x86_64, HVF on the arm64 dev host) unless it names another shape.

**Exit gate**
- [ ] clang and lld from the §17.7 image run on vibeOS unmodified and produce working binaries
- [ ] the pinned nightly's `rustc` and `cargo` from the §17.7 image run on vibeOS unmodified
- [ ] the vibeOS source tree builds on vibeOS into a bootable ISO
- [ ] that ISO boots and rebuilds itself for two generations, and the loop script's comparison reports the first- and second-generation ISOs byte-identical (§10.2, §17.5)
- [ ] `make check` and `make test` pass on vibeOS, itself a guest under QEMU with KVM: the harness on §17.6's CPython drives §17.6's QEMU under TCG and reports results the same way CI does. A test that needs something vibeOS does not yet offer as a harness host skips with a reason naming the line that adds it (§15.10's tap-device tests name §21.7's `/dev/net/tun`), and the harness fails if the skipped set differs from the on-device skip list this line writes into DESIGN §8.6, one entry per skipped test naming the line that adds what it needs
- [ ] the loop runs on aarch64 as well as x86_64, and each architecture can cross-build the other
- [ ] the loop script runs the whole loop from a clean checkout through the second-generation comparison with no manual step, and the job that checks the byte-identical line above runs only that script
- [ ] unmodified `gdb` and `strace` from the §17.7 image debug and trace a multithreaded C program on both architectures: a breakpoint, a backtrace, a single step, a hardware watchpoint, and `strace -f` across `clone`
- [ ] tag `phase-17` and cut the next release

### 17.1 POSIX completeness
- [ ] audit against what a real toolchain needs, and close the gaps rather than guessing
- [ ] filesystem behavior compilers depend on: `rename` atomicity, `O_TMPFILE`, `fsync` semantics, correct `mtime`
- [ ] process behavior: `posix_spawn`, large environments, long argument lists, pipe-heavy pipelines
- [ ] memory behavior: large `mmap`, `mprotect` for JIT, hundreds of thousands of small allocations
- [ ] `/proc` entries that build systems read
- [ ] a large-file test, since compilers write big object files and every off-by-one shows up there

### 17.2 C toolchain
- [ ] clang and lld from the §17.7 image, run unmodified, with `musl-dev`'s headers and crt objects from Alpine's `build-base`; rebuilding them from source on vibeOS is Phase 24
- [ ] `llvm-objdump` and `llvm-nm`, which the build calls, from the pinned nightly's `llvm-tools`, and `llvm-ar`, `llvm-strip`, and `llvm-readelf` from Alpine's LLVM, all in the §17.7 image
- [ ] `make`, `sh`, and `awk` from the §17.7 image, enough that an autoconf `configure` script runs to completion
- [ ] `cmake` and `ninja` from the §17.7 image, which most real projects assume

### 17.3 Rust toolchain
- [ ] `cargo` fetches the registry and git dependencies over Phase 15's TCP, with the TLS and libgit2 it links itself and the CA bundle in Alpine's minirootfs; the build loop itself runs offline from `cargo vendor` output (§17.7)
- [ ] the kernel itself builds on-device for `x86_64-unknown-none` and `aarch64-unknown-none-softfloat`, with no `build-std`; the user crate builds for its bare targets, and hostlib's tools for the Linux musl host triple, where they run like any other Linux binary

### 17.4 Development environment
- [ ] `git` from the §17.7 image, unmodified: clone, commit, branch, and push over HTTPS on Phase 15's TCP, through Alpine's libcurl and OpenSSL rather than §15.11's TLS
- [ ] `vim` from the §17.7 image, unmodified, on the serial console and over §15.8's `sshd`
- [ ] the debugger interface is Linux's `ptrace`, so `gdb` from the §17.7 image runs unmodified: `TRACEME`, `ATTACH`, `SEIZE`, `INTERRUPT`, `LISTEN`, `SETOPTIONS`, `PEEKDATA` and `POKEDATA`, `GETREGSET` and `SETREGSET` with `NT_PRSTATUS` and `NT_PRFPREG` (plus `NT_X86_XSTATE` on x86_64 and `NT_ARM_TLS` on aarch64), `GETREGS`, `SETREGS`, `GETFPREGS`, and `SETFPREGS` on x86_64, where gdb uses them, `CONT`, `SYSCALL`, `SINGLESTEP`, `GETEVENTMSG`, `GETSIGINFO`, `KILL`, and `DETACH`, and the `TRACESYSGOOD`, `TRACECLONE`, `TRACEFORK`, `TRACEVFORK`, `TRACEVFORKDONE`, `TRACEEXEC`, `TRACEEXIT`, and `EXITKILL` options, since `SEIZE` refuses an option it does not know and `strace -f` sets `TRACEVFORK`; `ATTACH` and `SEIZE` pass Linux's credential and dumpable checks (§13.9), and register writes go through the §13.8 context validation, and a `SETREGSET` of `NT_X86_XSTATE` whose XSAVE header fails it returns `EINVAL`, as on Linux
- [ ] hardware breakpoints and watchpoints for the tracer: the debug registers through `PEEKUSER` and `POKEUSER` on x86_64, `NT_ARM_HW_BREAK` and `NT_ARM_HW_WATCH` on aarch64; the tracee's memory read and written through `/proc/<pid>/mem` and `/proc/<pid>/task/<tid>/mem`, where gdb writes its breakpoints, and read through `process_vm_readv`; `/proc/<pid>/mem`, `process_vm_readv`, and `PEEKDATA` pin the tracee's address space as §13.9's `maps` does
- [ ] `POKEDATA` and `/proc/<pid>/mem` writes resolve a read-only tracee page through the §12.2 fault path with write intent and force, which breaks COW into a private anonymous copy, and never write a page-cache or COW-shared frame through the physmap; a test sets a breakpoint in a running binary's text through each interface, and the file's bytes and a second process running the same binary are unchanged (F023)
- [ ] user debug state is per-thread, as DESIGN §7.5's Debug state paragraph assigns it: outside the §18.4 detector build, the context switch loads DR0-DR3 and the DR7 the kernel built (below) for a thread with a breakpoint armed and clears DR7 for one without, and on aarch64 loads the `DBGBVR`/`DBGBCR` and `DBGWVR`/`DBGWCR` pairs and sets `MDSCR_EL1.MDE` only for such a thread, with `MDSCR_EL1.KDE` 0; `PTRACE_SINGLESTEP` sets TF in the thread's saved RFLAGS on x86_64, and on aarch64 sets the thread's saved `SPSR.SS` and, only on its return to EL0, `MDSCR_EL1.SS`, which every entry from EL0 clears before it clears `PSTATE.D`. Tests: a watchpoint armed in one process does not stop a second process writing the same address on the same CPU; on aarch64 (TCG per push, HVF on the dev host), a tracer single-steps a process 10,000 times while an untraced process pinned to the same CPU with §13.10's `sched_setaffinity` runs a loop and exits 0 with no `SIGTRAP`
- [ ] tracer writes stay in user space: `POKEUSER` of a debug-register address, and `SETREGSET` of `NT_ARM_HW_BREAK` or `NT_ARM_HW_WATCH`, refuse an address at or above `USER_MAP_END` with `EINVAL`, and `SETREGS` or `POKEUSER` of an `fs_base` or `gs_base` at or above it returns `EIO`, as Linux does at `TASK_SIZE_MAX`
- [ ] tracer-written debug controls are decoded, never loaded, as Linux decodes them. On x86_64, `POKEUSER` of DR7 drops the reserved bits (10 to 15, GD among them, and 32 to 63), ignores GE and LE, and takes L or G as a slot's enable; it returns `EINVAL` only for RW=10 (I/O), an execute slot whose LEN is not 1 byte, or a slot address not aligned to its length, and otherwise keeps the tracer's masked value for `PEEKUSER` and builds, from the decoded slots, the DR7 the switch loads. `PEEKUSER` and `POKEUSER` of DR6 read and write the thread's virtual DR6 (DESIGN §7.5); DR4 and DR5 return `EIO`. On aarch64, `SETREGSET` of `NT_ARM_HW_BREAK` and `NT_ARM_HW_WATCH` decodes each control word's enable, type, and byte-address select, ignores its privilege, `HMC`, and `SSC` fields, and encodes EL0, an unlinked address match, and no `MASK` itself, returning `EINVAL` only where Linux's decode does. In the §18.4 detector build both return `ENOSPC`, as Linux does when its slots are full. Tests: gdb's own DR7 for a watchpoint (L0 and LE, with RW and LEN set) is accepted; DR7 values with GD or bit 40 set reach no hardware, and the child keeps running with the kernel up; RW=10 returns `EINVAL`; the same static test runs under §13.11's differential runner on the x86_64 runner and matches Linux
- [ ] the kernel ignores a user breakpoint or watchpoint it hits itself, as Linux does, since x86 debug registers match at CPL 0: a `#DB` taken at CPL 0 whose saved DR6 names only slots the current thread's tracer armed clears them and continues, and one with DR6.BS set clears TF in the saved frame, logs once, and continues (DESIGN §2.5, §5.2); on aarch64 `MDSCR_EL1.KDE` is 0 outside the detector build, so an EL0 watchpoint that an `LDTR`/`STTR` accessor (§18.3) matches raises nothing. Tests on both architectures: gdb sets a write watchpoint on a buffer the tracee passes to `read`, the kernel stays up, and gdb stops only at the tracee's own later write to it; on the KVM leg, a data breakpoint on a `mov ss` operand followed by `syscall`, and again followed by `int3`, leaves the kernel up, the `syscall` returns its result, and the `int3` stops the tracee with `SIGTRAP`
- [ ] virtio-fs or 9p to mount the host checkout, so the build loop does not start by copying the tree into an image
- [ ] `strace` from the §17.7 image, unmodified, over the same `ptrace` with `PTRACE_GET_SYSCALL_INFO`, following threads and children with `-f`

### 17.5 The loop
- [ ] a build script that goes from a clean checkout to a bootable ISO on vibeOS
- [ ] a second-generation build, compared byte for byte against the first by the loop script
- [ ] the test suite running on-device
- [ ] timings recorded for each architecture in the Phase 17 guest, because "it works" and "it works in under an hour" are different claims; on the hosted runner each step of the loop (a generation's build, its boot, the on-device `make check` and `make test`) runs in one job under the 6-hour limit, or is split into jobs that hand the ISO and the build tree on as artifacts (§10.1)
- [ ] documented in `docs/` as a reproducible procedure, since the point is that someone else can do it

### 17.6 Build and test dependencies
Everything the build and the harness run on the host today must run on vibeOS.

- [ ] CPython from the §17.7 image, since the harness, `scripts/gen_ksyms.py`, and the `scripts/check_*.py` guards are Python; running it is also a hard test of the POSIX surface
- [ ] `xorriso` from the §17.7 image, and the Limine host tool built on vibeOS with §17.2's clang from the §17.7 image's Limine clone, as `setup.sh` builds it
- [ ] QEMU and the OVMF and aarch64 edk2 firmware that the §10.2 probe locates, from the §17.7 image, running under TCG, since vibeOS has no hypervisor until Phase 21; the harness gains a backend for the §21.2 VMM when that lands
- [ ] harness timeouts scaled for nested TCG, set once in the `VIBEOS_*` reader rather than per test

### 17.7 Upstream toolchains first
The loop closes first on toolchains their upstream projects already build for Linux, run unmodified
through §13.10 and §14.9. All of them are musl-hosted, so glibc (Phase 23) stays off this path. Rebuilding
them from source on vibeOS is Phase 24, and this phase does not wait for it. Every compiler, linker, and
test tool in the loop still runs on vibeOS.

- [ ] the pinned nightly's `rustc` and `cargo`, the `rustfmt`, `clippy`, `llvm-tools`, and `rust-src` components that `make check` and the build use, and `rust-std` for the host and for `x86_64-unknown-none`, `aarch64-unknown-none-softfloat`, and `aarch64-unknown-none`, as rust-lang publishes them for the `<arch>-unknown-linux-musl` host (Tier 2 with host tools on both architectures), run on vibeOS under §14.9's musl. The pin stays a nightly because the kernel uses nightly features (`abi_x86_interrupt`, `alloc_error_handler`); Rust code with `std` targets `*-unknown-linux-musl`, not a vibeOS triple (§24.3)
- [ ] a `vibeos-build` disk image holding that toolchain; the pinned §14.9 `minirootfs` and the whole package snapshot, so the packages §17.2, §17.4, and §17.6 name and those `make test`'s tiers build images from are local; a clone of the Limine binary branch at `setup.sh`'s pinned `LIMINE_TAG` and `LIMINE_COMMIT`, which the loop script passes to `setup.sh` as `LIMINE_REPO`; a `cargo vendor` tree for the workspace; and the §14.10 source archives the build reads (musl and compiler-rt for `make sysroot`), pinned by hash, built once by the host and stored by content hash as §13.11 stores its corpus, with the corresponding source of every copyleft binary the image holds published beside it as §14.10's policy requires, since the Actions cache holds 10 GB per repository. A release file must be under 2 GiB, so the image is kept zstd-compressed in parts below that, which a run streams through `zstd -d` into the image and checks against that hash; the on-device loop begins with neither a package install nor a download. The image's compressed and uncompressed sizes are recorded in DESIGN §8.6, and the image, the checkout, and the guest's disks fit the 14 GB disk GitHub documents for a hosted runner
- [ ] `cargo build --offline` from that image, with `flock` and `fcntl` locks (§13.9) held across parallel builds, and `-j` equal to the guest's CPU count, 4 in the Phase 17 guest
- [ ] no upstream binary is patched, wrapped, or `LD_PRELOAD`ed to run; each workaround is a kernel fix with a regression test in the cheapest tier that catches it
- [ ] `docs/UPSTREAM.md` lists every upstream bug hit (Limine, QEMU, edk2, musl, Alpine, rustc, LLVM), each with a link to the upstream issue or patch

---

# Era IV. Frontier

The parts that separate a working system from a serious one.

Every phase here needs 17: §18.6 extends its `ptrace`, §19.3 times its kernel build, §20.8 runs its
on-device `make test` nightly, §21.2 runs its QEMU as the VMM, and §22.4 builds releases with its
toolchains. Phases 18 and 19 are independent of each other and run under QEMU.

Phase 20 needs both architectures from 11, the drivers from 7 and 15, and §16.4's input abstraction,
which USB HID delivers into. It also extends three sections of 19: deeper C-states and frequency scaling
from ACPI extend the §19.6 idle path, NVMe polling follows §19.8, and aarch64 NUMA takes §19.7's code
path. Its interrupt routing above APIC ID 254, its IORT parsing of the SMMUv3, and §20.9's `q35` harness
machine build on §18.1, its microcode loader runs before §18.3's mitigations read CPUID, and §20.7's
TPM2 table and §20.8's swtpm serve §18.7's TPM driver, so it needs those three sections of 18.

Phase 21 needs 20 for §20.7's aarch64 ACPI, which its Hyper-V detection reads, and §20.8's records of
which hosted runners offer a guest VMX or SVM, which its nested job reads; 18 for the capabilities,
`seccomp`, mount namespaces, and resource limits its containers build on (§18.6), the KCOV coverage its
hostile-guest fuzzer steers by (§18.5), and the split-irqchip `q35` configuration its `/dev/kvm` serves
(§18.1); and 19 for the §19.3 benchmarks in its gate and the §19.4 group scheduling behind its cgroups.
Phase 22 needs 16, whose compositor its live image's desktop runs on (§22.2), and 18 to 21.

## Phase 18: Hardening

**Goal.** Assume everything in userspace is hostile and the kernel has bugs. Make both survivable.

**Unlocks.** Trusting the machine with anything that matters. Running untrusted code on purpose, which
Phase 21's containers and Phase 22's users both are.

**Architectures.** Both, except the `FSGSBASE` gate line and its §18.3 box, which are x86_64 only:
aarch64 keeps the kernel's per-CPU base in a register EL0 cannot write (§11.4), so it has no `swapgs`
hazard, and the §18.3 `lfence` box is x86_64 only for the same reason. The §18.3 KPTI,
branch-target-injection, IBPB, `verw`, SRSO, GDS, and ITS boxes are x86_64 only, and so is the vulnerabilities-report gate line,
since it runs on the §10.1 KVM leg, which is x86_64 only. The §18.2 PML4-slot box is x86_64 only, since aarch64 maps the kernel half through TTBR1, which no address space copies. The §18.3 aarch64 speculation box is aarch64 only, the counterpart of those x86_64 boxes. The §18.3 Speculative Store Bypass box is both. The §18.3 `LDTR`/`STTR` box is aarch64 only, and so are the MTE box in §18.4 and its gate
line: x86_64 has no shipping equivalent. Interrupt remapping in §18.1 is x86_64 only, since the GICv3
ITS already binds each MSI to its device ID, and so is the RMRR/IVMD box, whose regions come from x86
ACPI tables; IORT's RMR nodes, their aarch64 counterpart, arrive with §20.7. SMEP, SMAP, and UMIP
landed with S1 (§9.1), their fault
tests with §10.6, and PAN and PXN in §11.6. Control-flow integrity (CET, pointer authentication, BTI) is
the per-architecture stretch in §18.9.

**Exit gate**
- [ ] kernel `.text` read-only and executable, `.rodata` read-only and NX, and `.data` NX at the image VA; every other mapping of their frames, the physmap included, NX, and read-only for `.text` and `.rodata`; the §18.1 page table walker checks each of these mappings in an in-guest test (F105)
- [ ] KASLR active on both architectures: the harness boots one ISO three times and reads at least two distinct `KERNELOFFSET=` values from the VMCOREINFO notes of their `dump-guest-memory` cores
- [ ] with KASLR on, every frame of a deliberate panic's backtrace, and of the §10.7 core tool's report from a `hang_test` core, names the symbol and offset that the kernel ELF's own symbol table gives for its unslid address; the harness checks each frame, not only the first (F084)
- [ ] `FSGSBASE` enabled; an in-guest test sets the user GS base to a kernel-half value and enters the NMI, `#DB`, and `#MC` handlers from kernel mode with that base live, and on a 2-CPU guest runs a syscall loop while the other CPU sends NMI IPIs; in every case the handlers find this CPU's `PerCpu` and the user's value survives
- [ ] every speculation mitigation the kernel reports enabled at boot has a measured-cost entry in `docs/` for each CPU model it was reported on, measured in a 2-vCPU, 512 MiB guest under KVM on the hosted x86_64 runners, which draw their CPU model at random per job (§10.1), and under HVF on the aarch64 dev host; a harness test compares the boot log's list against the document's list for the model it booted on, and fails on a model with no list until one is measured
- [ ] on each CPU model the §10.1 KVM leg draws, the files under `/sys/devices/system/cpu/vulnerabilities/` in a 2-vCPU, 512 MiB vibeOS guest read the same as in Alpine's `linux-virt`, booted in the same shape in the same job, except for differences `docs/` lists with a reason, and where vibeOS reports `Vulnerable` and `linux-virt` a mitigation, that reason is a link to the §18.8 known escalation path that names the owner decision accepting it (DESIGN §2.10), which the harness comparison checks; `linux-virt` comes from the §14.9 mirror, added to its pin list (F024, F131, F132)
- [ ] syzkaller with KCOV coverage (§18.5) accumulates at least 24 hours of fuzzing per architecture each week on the weekly schedule, in shards of at most 5.5 hours (§10.1) that carry one corpus, each guest 2 CPUs and 512 MiB under TCG, with no open crash; the job summary records the total and the corpus coverage per subsystem, and every crash becomes a replayed regression test (§18.5)
- [ ] the §12.1 KASAN build passes the full test suite, userspace included, on both architectures
- [ ] with the §18.1 IOMMU on, QEMU's `edu` device programmed to DMA outside its mapped buffer is stopped and reported with the device and address, under `intel-iommu` and `amd-iommu` on `q35` and `iommu=smmuv3` on aarch64 `virt`; the virtio-blk and virtio-net in-guest tests pass through each
- [ ] a process confined by a `seccomp` filter, a mount namespace under `pivot_root`, `PR_SET_NO_NEW_PRIVS`, and an empty capability set cannot open a file outside its new root, create a socket of a family its filter denies, or regain a capability by executing a set-user-ID binary; an in-guest test tries each on both architectures
- [ ] `docs/THREAT_MODEL.md` has the parts §18.8 names; `scripts/check_threat_model.py` in `make check` fails when a part is missing, or when a known open escalation path does not link to an open box, a [Beyond](#beyond) entry, or a [funded goal](#funded-goals)
- [ ] libseccomp's live tests and the kselftest `seccomp_bpf` program, built static from pinned sources in the §13.11 Alpine container and cached by source hash, pass in-guest on both architectures, each with a checked-in skip list under 10% of its cases whose entries name a reason
- [ ] the §18.4 data-race detector build runs the full in-guest ladder at `-smp 4`, under KVM on x86_64 and TCG on aarch64, with §17.4's hardware breakpoint and watchpoint tests expecting `ENOSPC` and its single-step tests unchanged; it reports a seeded race on a plain shared counter within 10 seconds with the sampler pinned to that counter's increment while 8 threads switch on every CPU, counts no detector hit at CPL 3 and no dropped user address, and every other report is fixed or annotated at its site
- [ ] aarch64 under TCG with `-machine virt,acpi=off,mte=on -cpu max`: a use-after-free and a linear overflow in the kernel heap and in a user allocation tagged through `PROT_MTE` each raise a tag-check fault naming the allocation, and the full in-guest ladder passes with tagging on
- [ ] tag `phase-18` and cut the next release

### 18.1 Kernel memory protection
- [ ] per-section permissions applied after boot, with the init sections freed or made NX
- [ ] no writable-and-executable kernel mapping anywhere, including the trampoline page §10.6 left read-only and executable, asserted by a page table audit at boot
- [ ] every physmap PTE has NX set, checked by the boot audit in the box above; an executable physmap would make every user page executable in ring 0 through its physmap alias
- [ ] no writable alias of kernel code or read-only data: the physmap maps the image's physical span NX, with the image's per-section write permissions (`.text`, `.rodata`, and `.limine_requests` read-only; `.data` and `.bss` writable), split to 4 KiB at the image's unaligned edges, and the boot audit checks every VA that maps an image frame, not only the image VA; DESIGN §4.3's flag table gains the physmap image rows (F105)
- [ ] guard pages on every kernel stack, including the IST stacks and the KVA stack §10.6 moves boot onto from Limine's unguarded stack; the page table walker in the next box finds the page below each stack it reaches from the TCB table, each TSS, and each IST slot unmapped (F072)
- [ ] a page table walker that verifies the whole address space against a policy, run as an in-guest test
- [ ] an IOMMU behind the §6.4 translation interface: VT-d and AMD-Vi on x86_64 (a q35 harness configuration, alongside the `pc` default, with `-device intel-iommu,intremap=on` or `-device amd-iommu,intremap=on,pt=off`, plus `dma-remap=on`, which needs QEMU 10.2 or later, since without it QEMU's amd-iommu passes DMA through untranslated; `kernel-irqchip=split` under KVM) and SMMUv3 on aarch64 (`-machine virt,acpi=off,iommu=smmuv3`). Each device gets its own DMA domain, and virtio devices, started with `iommu_platform=on` so QEMU offers `VIRTIO_F_ACCESS_PLATFORM`, negotiate it; without it their DMA bypasses the IOMMU; each device the IOMMU translates names it as a supplier and gets its domain before its first probe, when the device is added if the IOMMU is registered and otherwise when the IOMMU registers, and a probe that would run first returns `Defer`; the binder retries deferred devices after each bind and, at the end of boot, logs each device still deferred with the supplier it waits for (DESIGN §12.2). In-guest on the q35 IOMMU configuration, a `kernel_tests` hook registers the IOMMU only after the PCI scan, and virtio-blk binds after it, with its domain attached
- [ ] a DMA address mask per device in the §6.4 translation interface (`edu`: 28 bits, QEMU's default `dma_mask`), with IOVAs allocated inside it when the IOMMU is on; a mapping that cannot fit fails with an error, never a truncated address, and `test_dma_edu` passes with the IOMMU on and its IOVAs inside 28 bits (F030)
- [ ] the DMAR and IVRS tables and the device tree's `iommu-map` parsed in the portable half, host-tested and fuzzed like the other firmware tables, against QEMU's tables and the DMAR and IVRS tables of the real machines in the linuxhw/ACPI corpus (CC-BY-4.0; pinned by commit, fetched at test time, recompiled with `iasl`, never committed), whose RMRR and IVMD entries also test the reserved-region mapping below; IORT, which describes SMMUv3 on ACPI machines, arrives with §20.7
- [ ] devices that cannot be isolated from each other (behind a bridge without ACS, or sharing a requester ID) share a domain, and the grouping is recorded, since device assignment works by group
- [ ] the reserved regions named by DMAR's RMRRs and IVRS's IVMD blocks mapped into their devices' domains, since integrated graphics and USB legacy emulation DMA into them
- [ ] a translation fault reported with the device, address, and access kind, counted per device, and passed to the driver, which resets its device rather than hanging
- [ ] the virtio transport checks each used-ring id: an id at or above the queue size, or one that is not the head of a chain the driver posted (checked against a driver-private shadow of each head), marks the device broken instead of freeing a chain, and each harvest pass handles at most queue-size entries; host tests drive the §6.5 simulated device with duplicate, non-head, and out-of-range ids (F048)
- [ ] the virtio transport checks each capability: its offset plus length lies inside its BAR, and the notify capability has a length of at least 2 with each queue's notify offset plus 2 inside it; a host test drives the §6.5 simulated device with a short notify capability (F048)
- [ ] IOTLB invalidation in strict and batched modes. Strict is the default: an unmap returns only after the invalidation that covers it has completed, so a device cannot reach a buffer once its driver has taken it back. Batched is opt-in through `iommu.strict=0` on the §10.2 command line, Linux's option; it lets a device read and write an unmapped buffer until its batch is flushed, part of the DMA gap DESIGN §2.10's device row says this section closes, so making it the default is the owner's decision, not an agent's (DESIGN §2.10). In both modes a frame a device could reach returns to the buddy only after its IOVA is unmapped and the IOTLB invalidation covering it has completed (DESIGN §2.4): the DMA mapping keeps each page it maps, by its `DmaBuffer` or a unit count, until then, so batched mode defers the frame's release with the IOVA's. Both modes are measured on virtio-blk and virtio-net throughput with the IOMMU on and off, on the §10.1 KVM leg and under HVF on the dev host, and the numbers are recorded in `docs/` beside the baseline's default mode from its kernel configuration, with a `docs/LINUX.md` entry when that default is batched; those numbers are the evidence the owner weighs to make batched the default. In-guest, in strict mode, `edu` programmed to DMA to an IOVA after its unmap has returned is stopped and reported
- [ ] interrupt remapping on VT-d and AMD-Vi, so a device cannot forge an MSI to an arbitrary vector: an in-guest test has the `edu` device write an MSI whose remapping entry was never programmed, and no interrupt arrives. §20.1 routes to APIC IDs above 254 through it

### 18.2 KASLR
- [ ] the kernel linked as a static PIE on both architectures. On x86_64, drop the `relocation-model=static` and `-no-pie` rustflags, since PIE is the built-in target's default and Limine keeps the slid image inside the top 2 GiB the `kernel` code model needs. On aarch64, pass `-C relocation-model=pie` and `-C link-arg=-pie` explicitly. `-znorelro` stays. §0.1 and the rustflags paragraph in DESIGN §3.1 are updated in the same commit
- [ ] `kaslr: yes` in `limine.conf`, written explicitly because Limine's default changed across major versions, so Limine picks the slide and applies the relocations before `_start`; the kernel takes its virtual base from Limine's executable address response, which `BootInfo` captures again for this (it was dropped in #91 while nothing read it), and never computes one
- [ ] the slide's entropy described in the threat model: Limine, at §11.1's pin, draws the slide, and the HHDM offset `kaslr: yes` also randomizes, from `RDSEED`/`RDRAND` on x86_64 and `RNDR` on aarch64, then from the firmware's `EFI_RNG_PROTOCOL`, and falls back to a PRNG seeded from the TSC or the generic counter only when neither answers. `RNDR` is optional and absent on Apple M-series, Cortex-A76, and Neoverse N1, so the aarch64 harness attaches `virtio-rng-pci`, over which edk2 offers `EFI_RNG_PROTOCOL` under HVF on the dev host; the documented gap is a machine with neither source. Limine does not report which source it used, so a boot line records only whether the CPU has `RDSEED`/`RDRAND` or `RNDR`
- [ ] randomized physmap and KVA region bases: the physmap is Limine's HHDM, which the kernel has adopted since §11.1, and whose offset Limine slides from the same sources (`randomise_hhdm_base: yes` written beside `kaslr: yes`), and the heap, KVA allocator, and `ioremap` bases are drawn from Limine's Entropy response, captured in `BootInfo`; DESIGN §4.1's region table and its "fixed, not discovered" rule are updated in the same commit
- [ ] every kernel-half PML4 slot a region can use exists before the first user address space, since `AddressSpace::new` copies PML4[256..512) once: `paging_init::install` allocates the PDPTs of the physmap, heap, KVA, and `ioremap` slots at their randomized bases, each slot a region can grow into included, and the kernel mapper panics if it would allocate a PML4-level table once an `AddressSpace` exists; DESIGN §4.1 states the rule (F101)
- [ ] the slide (`BootInfo`'s kernel base minus the link base) recorded; ksyms and the backtrace subtract it, so a crash dump symbolizes, and it is not otherwise exposed inside the guest; when QEMU's `vmcoreinfo` device is present the kernel writes it in a VMCOREINFO note (`KERNELOFFSET=`), which `dump-guest-memory` puts in the harness's cores, so the §10.7 core tool finds it
- [ ] the §18.8 threat model's known escalation paths list how user mode recovers the slide: prefetch and TLB timing reveal it while the kernel half is mapped in every user CR3, and with §18.3's KPTI on, the entry area that stays mapped still reveals it (EntryBleed, CVE-2022-4543) (F133)
- [ ] user address space randomization: stack, heap, mmap, and vDSO bases, and the `ET_DYN` load biases of §13.10 and §14.2, drawn from the §14.7 pool

### 18.3 CPU features

S1 turned SMEP, SMAP, UMIP, and `CR0.WP` on at boot (`arch::cpu::harden`). §10.6 put user access behind
`stac`/`clac` accessors, with the in-guest fault tests, and §11.6 did PAN and PXN. Each thunk and
instruction sequence below is written from the CPU vendor's published guidance (AMD's, Intel's, or
Arm's) and cites it, never from Linux's assembly (DESIGN §1.5). What is left:

- [ ] an `lfence` on both paths after every test that decides `swapgs`: the CS.RPL test in `arch::gs::enter` and in every entry stub, and §10.6's `GS_BASE` sign test on the IST vectors, so a mispredicted test cannot run kernel loads on the wrong GS base (CVE-2019-1125); the kernel build runs `scripts/check_entry.py`, which disassembles the kernel ELF with the pinned toolchain's `llvm-objdump` and fails on a conditional `swapgs` without an `lfence` on either path (F132, F133)
- [ ] `CR4.FSGSBASE` enabled. Once a user can `wrgsbase` a kernel-half value, the §10.6 sign check is unsound, so NMI, `#MC`, `#DB`, and `#DF` entry saves `GS_BASE` with `rdgsbase`, loads this CPU's `PerCpu` pointer (kept at the top of each per-CPU IST stack), and restores the saved value on exit. Every switch away from a user thread reads back the live user FS base and user GS base (the latter from `KERNEL_GS_BASE` while in the kernel), since `wrfsbase` and `wrgsbase` change them without a syscall. `CR4.FSGSBASE` is set only once the `lfence` box above has landed (F133)
- [ ] aarch64: user copies through the unprivileged `LDTR`/`STTR` instructions, so PAN is never cleared, even inside the accessors; an in-guest test asserts `PSTATE.PAN` is set inside every accessor
- [ ] CPU vulnerability enumeration on every CPU: CPUID.01H:EAX family and model, CPUID.(EAX=7,ECX=0):EDX, which `x86::cpuid_leaf7` discards today, CPUID.(EAX=7,ECX=2):EDX (`BHI_CTRL`, which enumerates `BHI_DIS_S`), `IA32_ARCH_CAPABILITIES` (MSR `0x10A`, read only when EDX bit 29 is set), and CPUID `0x80000008`:EBX on AMD; `MIDR_EL1` and `ID_AA64PFR0_EL1.CSV2` and `CSV3` on aarch64. The mapping from these bits to the mitigations below is in `vibeos-core`, host-tested against values recorded from each CPU model the §10.1 KVM leg has drawn and, for the aarch64 bits, from the dev host under HVF and the `-cpu` models the aarch64 harness runs under TCG; its result is logged at boot and exposed as Linux's `/sys/devices/system/cpu/vulnerabilities/` files. It covers every file the baseline release documents there (its `Documentation/ABI/testing/sysfs-devices-system-cpu`), and each file has a mitigation box in this subsection or §21.1, or a §18.8 known escalation path that names the owner decision accepting it (DESIGN §2.10); `l1tf`'s page-table part is §12.4's and §12.7's PTE inversion. The boxes below were written against recent LTS releases' lists, so a name the baseline does not document drops its box, and a file it adds gets one (F131, F133)
- [ ] KPTI on x86_64, on by default on an Intel CPU whose `IA32_ARCH_CAPABILITIES` is absent or has `RDCL_NO` clear, and forced by `pti=on` on the §10.2 command line: a process's user CR3 maps from the kernel half only a non-global entry area (the IDT, GDT, and TSS pages, each CPU's entry stack, and the entry stubs), and every entry from ring 3 goes through an asm stub that switches CR3 before its first kernel-data access; PCID tags both CR3s where CPUID reports PCID and INVPCID; each address space carries a flush generation that every round increments, and each CPU records per PCID slot the generation it last synced, so a switch into a space whose generation is newer flushes that PCID before user code runs, while a CPU still clears its bit in the space it leaves (DESIGN §7.9); the §12.3 loom model gains the generation check, with a mutant that skips it and must fail; DESIGN §7.5's CR3 row says so; DESIGN §4.1 lists what a user CR3 maps (F024)
- [ ] with `pti=on`, an in-guest test's user load from a kernel-half address takes a `#PF` whose error code has P clear, and `make test-kernel` passes with `pti=on` under TCG and on the KVM leg (F024)
- [ ] syscall-derived indices masked after their bounds check: `nospec_index(i, len)` in `vibeos-core` (`cmp`/`sbb`/`and` on x86_64, `csel` then `csdb` on aarch64), host-tested, applied to the syscall number before dispatch and to each fd, pid, and open-file index before it indexes a table (`proc.rs`, `proc_init.rs`, `file_init.rs`); DESIGN §2 states the rule (F025)
- [ ] each syscall entry zeroes the user general-purpose registers it saved before it calls into Rust, as Linux's `PUSH_AND_CLEAR_REGS` does, so no user value stays live in a register across dispatch (F025)
- [ ] branch target injection on x86_64: `IA32_SPEC_CTRL.IBRS` set on every CPU that enumerates `IBRS_ALL`, plus `BHI_DIS_S` where enumerated; the kernel is built with the pinned nightly's `-Zretpoline-external-thunk` and `-Zfunction-return=thunk-extern`, and its thunks in `arch/` are chosen once at boot, before §18.1's permissions apply: a retpoline without `IBRS_ALL`, a plain indirect branch with it, and on AMD Zen 1 and Zen 2 the untrained-return sequence AMD's Retbleed guidance describes (F131)
- [ ] an IBPB through `IA32_PRED_CMD` and an RSB fill when `switch_cr3_for` moves a CPU between two user address spaces, on a CPU that enumerates IBPB (F131)
- [ ] `verw` of a kernel data selector before every return to ring 3 (`sysretq` and each `iretq` to a user frame) on a CPU that reports `MD_CLEAR` (CPUID.(7,0):EDX[10]) and either lacks `MDS_NO` or has TSX and lacks `TAA_NO` (F132)
- [ ] Speculative Return Stack Overflow on AMD Zen 1 to Zen 4 (`spec_rstack_overflow`), which the hosted runners' EPYC CPUs are: the return sequence AMD's SRSO guidance gives for the CPU's family, a safe-RET thunk on Zen 3 and Zen 4, chosen at boot with the thunks above, or IBPB on kernel entry where the guidance prefers it, with the microcode check the guidance names; the file reports what the baseline reports on that CPU
- [ ] Speculative Store Bypass (`spec_store_bypass`) on both architectures: `prctl` `PR_SET_SPECULATION_CTRL` and `PR_GET_SPECULATION_CTRL` with `PR_SPEC_STORE_BYPASS`, in the baseline's default mode, where a process opts in, applied per thread at each switch through `IA32_SPEC_CTRL.SSBD`, or `VIRT_SPEC_CTRL` on an AMD CPU that offers only that, and on aarch64 through `PSTATE.SSBS` where `ID_AA64PFR1_EL1` reports it, otherwise the SMCCC `ARCH_WORKAROUND_2` call firmware reports; DESIGN §7.5's per-thread CPU state table gains its row, with the two-process in-guest test AGENTS.md rule 8 requires
- [ ] Gather Data Sampling on affected Intel CPUs (`gather_data_sampling`): the microcode mitigation left on and locked through `IA32_MCU_OPT_CTRL` where `IA32_ARCH_CAPABILITIES` enumerates `GDS_CTRL`; without that microcode the file reports what the baseline reports
- [ ] the `verw` of the box above also runs before every return to ring 3 on a CPU the enumeration finds affected by MMIO Stale Data (`mmio_stale_data`) or Register File Data Sampling (`reg_file_data_sampling`, with `RFDS_CLEAR` enumerated) on Intel, or by Transient Scheduler Attacks (`tsa`) on AMD Zen 3 and Zen 4, with the microcode each vendor's guidance names
- [ ] Indirect Target Selection on affected Intel CPUs (`indirect_target_selection`): the indirect-branch and return thunks chosen at boot keep their branch in the upper half of a 64-byte line, as Intel's ITS guidance describes, so no indirect branch or return the kernel executes sits where the CPU mispredicts it
- [ ] aarch64: on a CPU the enumeration marks affected by Spectre-v2 or Spectre-BHB, exception entry from EL0 runs the SMCCC `ARCH_WORKAROUND_1` or `ARCH_WORKAROUND_3` call that firmware reports; on a CPU whose `CSV3` is 0, EL0 runs with a TTBR1 that maps only the exception vectors and their trampoline, as Linux's `kpti` does (F133)
- [ ] `mitigations=off` on the §10.2 command line turns off the speculation mitigations of this subsection and §21.1, as the baseline's `mitigations=off` does: KPTI and aarch64's `kpti`, the `swapgs` `lfence`, index masking, register clearing, `IBRS` and `BHI_DIS_S`, retpolines and the return thunks, IBPB and RSB fill, `verw`, the Speculative Store Bypass control, the GDS microcode mitigation, and the SMCCC workarounds. It leaves on SMEP, SMAP, UMIP, PAN, PXN, the `FSGSBASE` entry handling, the `LDTR`/`STTR` accessors, and §12.4's and §12.7's PTE inversion, which are not speculation mitigations, so the exit gate's cost entries compare one image with and without them (F133)

### 18.4 Stack and memory safety
- [ ] stack canaries in both kernel and userspace. The kernel builds with `-Z stack-protector=strong`, and on the `*-none` targets LLVM checks one global `__stack_chk_guard`. It lives in `.data` and is written by inline assembly at the top of `_start`, before its first call: from Limine's entropy response (§18.2), otherwise from `RDSEED`, `RDRAND`, or `RNDR`, and failing all of them from the cycle counter, with a boot line that says the seed is weak. It is never rewritten, since a live frame would fail its check. A per-thread guard waits for rustc to expose LLVM's guard-register options, and the prebuilt `core` and `alloc` the kernel links carry no canaries; the box records both. `__stack_chk_fail` panics, and the dump's backtrace names the function. The §10.5 runtime builds the same way and seeds its guard from `AT_RANDOM` at the top of its `_start`; musl seeds its own from `AT_RANDOM`, and the §14.6 recipe tool builds C with `-fstack-protector-strong`. A `kernel_tests` case overruns a stack array in an instrumented function inside a `catch` scope and sees the panic, and a `/bin/tests` case does the same in user mode and is killed by a signal, never returning into the overwritten frame
- [ ] `_FORTIFY_SOURCE` for the C userspace through `fortify-headers`, as Alpine does, since musl has no fortify layer of its own
- [ ] a hardened `malloc`: guard pages, delayed reuse, randomized placement, double free detection
- [ ] the §12.1 KASAN build run over the full test suite, userspace included, on both architectures, with the kernel-side buffer of every §10.6 user copy checked explicitly, since the copy itself is assembly the compiler does not instrument
- [ ] a checked kernel build (`-C overflow-checks=on -C debug-assertions -Zub-checks=yes`, since rustc has no UBSAN) passing the full suite on both architectures; musl and the C userspace built with `-fsanitize=undefined -fsanitize-trap=undefined`, trap mode because no UBSan runtime exists on vibeOS; Miri over the portable crate's host tests is §10.8
- [ ] the `unsafe` audit: every block's `// SAFETY:` reason from the standing gate reviewed against the invariant it claims, since a pile of stale one-liners is where this ends up otherwise
- [ ] a sampling data-race detector build with no compiler instrumentation, since rustc has no ThreadSanitizer for the `*-none` targets: a build-time hostlib pass lists the kernel's plain memory-accessing instructions with their operand forms, excluding atomics, per-CPU accesses, and the functions the NMI, `#MC`, `#DB`, and `#DF` handlers run (on aarch64, the debug-exception and SError handlers), which the kernel places in a section the pass skips, as Linux skips `noinstr`; at run time a hardware instruction breakpoint (a debug register on x86_64, a `DBGBVR`/`DBGBCR` pair on aarch64) on a sampled one fires, so kernel text is never patched; the kernel computes the address from the trapped registers, arms a hardware data watchpoint on it on every other CPU, and stalls briefly. The detector owns every debug slot in its build (DESIGN §7.5): the switch never writes DR7, `MDSCR_EL1.MDE`, or `KDE` there, and ptrace's debug-register writes return `ENOSPC` (§17.4). The hostlib pass also excludes the §10.6 user-memory accessors and §18.3's `LDTR`/`STTR` accessors, whose addresses are the current process's user VAs; the arming step drops, and counts, any computed address below the kernel half; and a `#DB` taken at CPL 3 whose saved DR6 names only detector slots resumes with no signal and is counted
- [ ] no breakpoint or watchpoint fires inside the NMI, `#MC`, or `#DB` handlers: on x86_64 their entries save DR7 and clear it before anything else, as Linux's `local_db_save` does, since their §2.1 IST stacks do not nest; on exit `#DB` writes the detector's next configuration and NMI and `#MC` restore the saved one; a detector hit taken at CPL 0 or EL1 is reported and execution continues (DESIGN §2.5); on aarch64 the debug-exception and SError handlers keep the `PSTATE.D` that exception entry sets. Other CPUs are armed through a per-CPU request slot and a dedicated IPI (an SGI on aarch64), added to DESIGN §7.6 in the same commit and sent without waiting for acknowledgement, never through the §4.9 call-function slot, which the interrupted code may hold; a target that takes the IPI after the stall drops the stale request, and one with interrupts off for the whole stall misses that sample
- [ ] the detector reports both accesses with backtraces when another CPU hits the watchpoint during the stall, or when the value changed with no hit; an intentional race is annotated at its site with the reason, and only the annotation suppresses it
- [ ] aarch64 memory tagging (MTE) as tag-based KASAN: a build that tags kernel heap and buddy allocations at the 16-byte granule, retags on free, and checks synchronously, reporting a tag-check fault like a KASAN report; `TCR_EL1.TBI1` for the kernel's tagged pointers; `PROT_MTE` and `PR_SET_TAGGED_ADDR_CTRL` for userspace with `TBI0`, and syscalls from a process that set `PR_TAGGED_ADDR_ENABLE` clear the tag byte of each user pointer before the §10.6 range check, which an in-guest test checks by passing a tagged buffer to `read`; tested under TCG with `-machine virt,acpi=off,mte=on -cpu max`, since HVF does not expose MTE

### 18.5 Fuzzing
- [ ] Linux's KCOV ABI: the kernel built with SanitizerCoverage `trace-pc` instrumentation (`-C passes=sancov-module` with `-sanitizer-coverage-level=3` and `-sanitizer-coverage-trace-pc` through `-C llvm-args`), `kcov` in a `debugfs` mounted at `/sys/kernel/debug`, mode 0700 and owned by root, as Linux mounts it, with `KCOV_INIT_TRACE`, `KCOV_ENABLE`, and `KCOV_DISABLE`, and a per-thread buffer the fuzzer maps; the §13.13 fuzzer reads the same buffer as its coverage signal. PCs are recorded with the §18.2 slide subtracted, as Linux's are. The `__sanitizer_cov_trace_pc` callback is written in `global_asm!`, so it is not itself instrumented, and records only in thread context for a thread that enabled KCOV, doing nothing before per-CPU data exists
- [ ] syzkaller's `linux` target runs against vibeOS with `sandbox: none`, an `enable_syscalls` list generated from the §10.5 table, and its Linux-only features off; its executor is built static on the Linux host and runs unmodified. A `vibeos` target is written only if that fails, and this line records which
- [ ] the §15.8 `sshd` gains what syzkaller's QEMU backend uses: public-key user authentication from `~/.ssh/authorized_keys`, since its `ssh` runs with `BatchMode=yes`; root login with a key; `exec` requests without a pty; remote port forwarding (the `tcpip-forward` request and `forwarded-tcpip` channels behind `ssh -R`), since the backend runs QEMU's user network with `restrict=on` and the executor reaches the manager only through that tunnel; and a native `scp` in the base system whose `-t` sink mode is what `scp -O` runs; each tested from a host client on both architectures; the key syzkaller logs in with is generated per run and reaches root's `authorized_keys` only through §14.3's harness overlay
- [ ] syzkaller's QEMU backend boots a build with KCOV and the §12.1 KASAN on both architectures and reaches the guest through that `sshd`, including its reverse-forwarded port and its file copy with legacy-protocol `scp -O`; the C reproducer syzkaller writes for every crash is checked in and replayed by the user test runner
- [ ] a hostlib tool maps the corpus's KCOV program counters to source files through the kernel ELF's line table and reports coverage per subsystem on every run, so a subsystem the fuzzer never reaches is visible
- [ ] syzkaller is the weekly campaign, in the shards the exit gate describes, with one corpus artifact carried from shard to shard; the §13.13 fuzzer stays on the nightly job, and the parser-level fuzzers run from §10.2 (every byte-slice parser, ELF included) and §15.10 (network)
- [ ] fuzzed FAT and vibefs images, and fuzzed MBR, EBR, and GPT partition tables, attached as virtio-blk disks, scanned and mounted in-guest under the §12.1 KASAN build, on the weekly job (F117)
- [ ] vibefs v2's read-time validation (VIBEFS.md §15) refuses every defect the kernel review found v1's mount accepting: a leaf count over capacity, a root, child, or extent pointer outside the volume, an extent whose end overflows or passes the volume, inline data larger than the inline area, a duplicate inode number or one at or above the next free one, a metadata block referenced twice, and a reachable block whose refcount is 0; each returns `Corrupt`, and host tests build a v2 image with each defect and mount it, under the fuzzed image mounts of the box above (F061)
- [ ] the §15.10 in-guest packet fuzzer also runs in a §12.1 KASAN build, on the weekly job
- [ ] malformed ELF files exec'd in-guest under KASAN, each refused with an error and none panicking the kernel
- [ ] every crash the fuzzers find becomes a replayed regression test in the cheapest tier that catches it

### 18.6 Access control
- [ ] the privilege checks audited: a generated in-guest test, run as uid 1000, calls every syscall in the §10.5 table, issues every `ioctl` in the §13.9 registry, and opens every device node devfs creates, each against an object or with an argument that user may not use, and expects the error Linux returns there (`EACCES` or `EPERM`, or the bound's `EAGAIN`, `ENOSPC`, or `EMFILE`); each table, registry, and devfs entry names its check and bound, as the standing gate requires, and the generator fails on one that names no check
- [ ] capability-style privilege splitting through Linux capabilities: `capget` and `capset`, the permitted, effective, inheritable, bounding, and ambient sets with Linux's `execve` rules, and every §13.9 privilege check naming the capability it needs, so uid 0 holds the full set rather than bypassing the checks; the `prctl` controls libcap sets them through: `PR_CAPBSET_READ`, `PR_CAPBSET_DROP`, `PR_CAP_AMBIENT`, `PR_SET_KEEPCAPS`, and `PR_SET_SECUREBITS`
- [ ] file capabilities, which Linux's `execve` rules above read: the `security.capability` extended attribute in Linux's `vfs_cap_data` layout (revision 2; §21.5 adds revision 3's root uid with user namespaces) on vibefs v2 and tmpfs, settable only with `CAP_SETFCAP` and cleared by a write, as Linux clears it; unmodified `setcap` and `getcap` from the §14.9 mirror set and read it, and an in-guest test runs a binary carrying `cap_net_raw+ep` as uid 1000 and opens a raw socket
- [ ] the syscall filter is Linux's `seccomp`: `SECCOMP_SET_MODE_STRICT`, and `SECCOMP_SET_MODE_FILTER` with a classic BPF program over `struct seccomp_data` (whose `arch` is `AUDIT_ARCH_X86_64` or `AUDIT_ARCH_AARCH64`) checked by seccomp's own validator (aligned 32-bit loads inside `struct seccomp_data`, no socket-filter ancillary loads) and then run on the classic BPF interpreter behind §15.7's `SO_ATTACH_FILTER`; `SECCOMP_RET_KILL_PROCESS`, `KILL_THREAD`, `TRAP`, `ERRNO`, `TRACE` (a `PTRACE_EVENT_SECCOMP` stop through §17.4's `ptrace`, which gains `PTRACE_O_TRACESECCOMP` here), `LOG`, and `ALLOW`; filters inherited across `fork` and `clone` and kept across `execve`; `PR_SET_NO_NEW_PRIVS`, which a filter installed without `CAP_SYS_ADMIN` requires
- [ ] the rest of the interface libseccomp probes for and the kselftest `seccomp_bpf` program covers: `prctl(PR_SET_SECCOMP)` and `PR_GET_SECCOMP` beside the `seccomp()` call; `SECCOMP_FILTER_FLAG_TSYNC`, `FLAG_LOG`, and `FLAG_NEW_LISTENER` with `SECCOMP_RET_USER_NOTIF` and the `SECCOMP_IOCTL_NOTIF_RECV`, `SEND`, `ID_VALID`, and `ADDFD` ioctls; `SECCOMP_GET_ACTION_AVAIL` and `SECCOMP_GET_NOTIF_SIZES`
- [ ] mount namespaces through `unshare(CLONE_NEWNS)` and `clone(CLONE_NEWNS)`, and `pivot_root`, as Linux defines them; `setns`, `/proc/<pid>/ns/*`, and the other namespaces are §21.5
- [ ] resource limits enforced through §13.9's `prlimit64`: `RLIMIT_AS`, `RLIMIT_DATA`, `RLIMIT_STACK`, `RLIMIT_NPROC`, `RLIMIT_FSIZE` with `SIGXFSZ`, `RLIMIT_CPU` with `SIGXCPU`, and `RLIMIT_MEMLOCK` over §12.4's `mlock`; `RLIMIT_NOFILE` is §13.9's and `RLIMIT_CORE` §13.8's; `RLIMIT_SIGPENDING` is §13.8's, `RLIMIT_NICE`, `RLIMIT_RTPRIO`, and `RLIMIT_RTTIME` §19.4's, and `RLIMIT_MSGQUEUE` §23.1's
- [ ] audit records through Linux's `NETLINK_AUDIT` for privileged operations: each capability check of this subsection that grants or denies, `setuid`, `setgid`, `setreuid`, `setregid`, `setresuid`, `setresgid`, `capset`, `mount`, `umount2`, `pivot_root`, and each `seccomp` filter install; an in-guest test runs each operation once as root and once as uid 1000, where it is denied, and reads exactly one record for each run

### 18.7 Crypto and boot integrity
The primitives and the CSPRNG are §14.7, package signatures §14.6, and TLS §15.11. This is what needs a
boot chain.

- [ ] UEFI Secure Boot with vibeOS's own keys, not Microsoft's UEFI CA: `limine.conf` pins the kernel and initrd by hash, sets `editor_enabled: no` so the boot menu cannot change the command line, and is enrolled into Limine's EFI binary with `limine enroll-config`, which `sbsign` then signs with a db key, and the kernel ELF carries a signature by that key for §25.4's `kexec_file_load`; the release db key is held as §14.6's key-custody record holds the release key, with its certificate in the tree. A harness test on both architectures generates a throwaway PK, KEK, and db per run, enrolls them with `virt-fw-vars` into a per-run copy of the empty variable store of a Secure Boot edk2 build the §10.2 probe locates (OVMF's on `q35` with SMM, and on `virt` the distribution's AAVMF, since QEMU's bundled aarch64 edk2 has none), signs with that db, and shows the signed chain reaching `shell ready` and an unsigned Limine, an edited `limine.conf`, or a changed kernel or initrd refused; DESIGN §2.10's Limine row names what this chain covers and what it does not
- [ ] measured boot with a TPM (swtpm under QEMU on both architectures): a TPM 2.0 driver that reads PCRs, found through the ACPI TPM2 table (TIS or CRB) on x86_64 and the device tree's `tcg,tpm-tis-mmio` node on aarch64; the event log from Limine's TPM Event Log response (Limine 12.1 and later, which, with `measured_boot: yes` written in `limine.conf`, also measures the kernel, the initrd, the command line, and `limine.conf` itself), captured in `BootInfo`; the log is replayed against the PCRs, and its digests for the bootloader, kernel, initrd, and boot configuration are checked against boot-chain entries the build adds to the §14.6 signed release manifest, in the form the event log records them (for the EFI bootloader, the Authenticode image hash the firmware measures, not the file's SHA-256); firmware measurements are recorded but not checked, since the build does not produce the firmware; the check prints a registered marker, `boot: verified` only when the replayed log matches the PCRs, its boot-chain digests match the manifest, and the log's PCR 7 `SecureBoot` variable event records Secure Boot on, and otherwise `boot: unverified` with the reason (no TPM, Secure Boot off, a boot with no firmware such as §26.4's PVH entry, or a mismatch), and nothing else in the system claims a verified boot; the §10.2 fw_cfg append, which only a VMM writes, is extended into PCR 12, where systemd-stub measures a command line, with an event in the log, before anything reads a PCR
- [ ] full disk encryption with AES-XTS, added to the §14.7 facade over the `xts-mode` crate and RustCrypto's `aes`, with the IEEE 1619 vectors as host tests, and the key derived from a passphrase with §14.7's Argon2id; the transform is an encrypting block device stacked on any §7.1 `BlockDevice`, built here, which §29.1's mapping interface later drives

> **OWNER DECISION NEEDED (review J026)**: two questions about what Secure Boot covers.
>
> 1. Revoking a signed Limine. vibeOS's db key signs each release's Limine binaries. If one is later
>    found exploitable, only a KEK-signed `dbx` update, or the user in the firmware's key menu, stops it
>    from booting, and root on an installed system can put the old signed binary back on the ESP.
>    Options: (a) a project KEK, made and kept offline with you as the §14.6 root key is, which the
>    installer enrolls where the machine lets it (VMs, and machines whose keys are cleared to setup
>    mode), so a release can ship a KEK-signed `dbx` update; costs one more offline key and a signing
>    step at each revocation. (b) a KEK the installer makes per machine and keeps readable by root
>    there; costs nothing, but root on that machine can then sign any boot binary, so Secure Boot stops
>    only offline tampering. (c) no KEK: a signed Limine is revoked only by rotating the db key and
>    having users remove the old certificate in the firmware menu, and THREAT_MODEL lists that.
>    Recommended: (a) where the installer can enroll a KEK, and (c) on machines that keep their maker's.
>
> 2. What the verified chain covers. Secure Boot and measured boot cover the firmware's measurements,
>    the signed Limine, its enrolled configuration and command line, the kernel, the initrd, and a
>    kernel that `kexec_file_load` starts. Each slot's root (init, the updater, the package manager),
>    the shared state partition, and the key-set root record the updater trusts are outside it:
>    full-disk encryption hides them but does not authenticate them, and server and cloud images are not
>    encrypted, so someone who can write the disk while the machine is off can replace the updater and
>    its root record while every check still passes. Options: (a) accept this for 1.0, as most Linux
>    distributions do: THREAT_MODEL lists it under Known escalation paths, and a Beyond entry, sealed
>    slots, describes the fix and says it changes the installed layout; (b) sealed slots before Phase
>    22's first installable release: each slot root is a read-only image the release builds
>    reproducibly, checked block by block against a hash tree in dm-verity's documented format whose
>    root hash the slot's enrolled configuration pins, with the key-set root record inside it and
>    user-installed packages moved to the state partition; about a phase of work in Phase 22, and a
>    package manager that installs into two places. Recommended: (a).
>
> Until you answer, no KEK exists and nothing enrolls one, the slots are not sealed, and THREAT_MODEL
> lists both gaps as open with this block as their decision. Question 1 is needed before §18.7's Secure
> Boot box closes, and question 2 before Phase 22's first installable release; every box above works
> under each option.

### 18.8 Threat model
- [ ] `docs/THREAT_MODEL.md`, grown from DESIGN §2.10's trust-boundary table, which it then replaces as the threat model, with the headings `Trusted`, `Untrusted`, `Out of scope`, and `Known escalation paths`; each out-of-scope entry gives its reason, and each escalation path says why it is open, names the owner decision that accepted it (DESIGN §2.10's records move here with the table), and links to an open box, a [Beyond](#beyond) entry, or a [funded goal](#funded-goals); `scripts/check_threat_model.py` checks the headings and the links. It also has a `Security-relevant defaults` table: each sysctl, mount mode, and device-node mode that decides what an unprivileged process reaches, with vibeOS's default, the default of Linux's baseline release, and the line that set it. Earlier lines give its first rows (§13.9's `/dev/kmsg` rule, which is Linux's `kernel.dmesg_restrict` at 1, §13.9's dumpable rule, which is `fs.suid_dumpable` at 0, and §18.5's debugfs mode), and each later line that adds such a default adds its row, among them `kernel.perf_event_paranoid` and tracefs's mode (§19.1, §19.2), the real-time throttle (§19.4), `kernel.io_uring_disabled` (§19.8), `/dev/kvm` (§21.2), `user.max_user_namespaces` (§21.5), `/dev/uinput`, `/dev/uhid`, and `/dev/hidraw*` (§31.2), and `/dev/fuse` (§36.2). Each default is Linux's or stricter. A weaker one is the owner's decision (DESIGN §2.10), and its row links to it; one that differs from Linux's in either direction is also a `docs/LINUX.md` entry. `scripts/check_threat_model.py` checks the table's columns and fails on a weaker row with no link
- [ ] `SECURITY.md` names the reporting channel and who triages a report; §22.5 adds private vulnerability reporting, the embargo procedure, response times, and a drill

### 18.9 Stretch: control-flow integrity
- [ ] CET shadow stacks and indirect branch tracking on x86_64
- [ ] pointer authentication and BTI on aarch64

---

## Phase 19: Performance and Observability

**Goal.** Know why it is slow, then stop being slow. Neither is possible without measurement first.

**Unlocks.** Numbers instead of adjectives, and every later performance claim in this file.

**Architectures.** Both. Topology comes from CPUID on x86_64, and from MPIDR plus the device tree's
`cpu-map` on aarch64. §19.7 NUMA is x86_64 only in this phase: Limine strips the device-tree `memory@`
nodes that carry aarch64 memory affinity, so aarch64 NUMA arrives with ACPI SRAT on §20.7's ACPI `virt`.
Idle is `mwait` where CPUID reports MONITOR, `hlt` otherwise (KVM hides MONITOR unless QEMU runs with
`-overcommit cpu-pm=on`), and `wfi` on aarch64. No free host is known to give a guest hardware PMU
events: x86 TCG emulates no PMU, GitHub documents none for its runners, which are themselves VMs, and HVF
emulates at most a cycle counter. So the sampling path is checked on aarch64 under TCG, whose emulated
PMUv3 counts cycles, `SW_INCR`, and (under `-icount`) retired instructions and raises the overflow
interrupt, and on x86_64 from a timer-driven sampler. Hardware events (cache and branch misses, branch
records, precise sampling) need a physical machine and are in [Funded goals](#funded-goals). The
benchmark-threshold gate line is x86_64 only, since the §10.1 KVM leg is x86_64 and GitHub's arm64
runners have no KVM; the wakeup-latency, idle, idle-clock, lock-contention, and slab lines take their aarch64 numbers
under HVF on the dev host, running the §19.3 workloads they name there, as §10.9 dev-host records.

**Exit gate**
- [ ] a flamegraph produced from a counter-overflow sampling profile on aarch64 under TCG, and from a timer-driven sampling profile on x86_64 under KVM, both in a 2-vCPU, 1 GiB guest
- [ ] a live §19.1 trace of a `read` that misses the page cache shows its syscall entry, the virtio-blk submission, the completion IRQ, the reader's wakeup, and the syscall exit in order on one timeline, in an in-guest test on both architectures
- [ ] benchmarks with thresholds on the §10.1 KVM leg in a 4-vCPU, 1 GiB guest, and a regression fails that job and so blocks the phase tag
- [ ] scheduler wakeup latency at the 99th percentile under 1 ms with a CPU-bound load of four times the core count, measured under KVM in a 4-vCPU, 1 GiB guest
- [ ] an idle CPU takes fewer than 5 timer interrupts per second over one minute, measured under KVM in a 2-vCPU, 512 MiB guest
- [ ] over that idle minute, `CLOCK_MONOTONIC`, printed by an in-guest program at its start and end, advances within 0.5% of the host time between the two serial lines (F027)
- [ ] lock contention profiled; the most contended lock's spin time on the §19.3 mixed interactive workload cut by at least half, under KVM in a 4-vCPU, 1 GiB guest, with before and after numbers recorded
- [ ] slab statistics show per-cache object counts, and buddy-lock spin time on the §19.3 allocation microbenchmark, run on every CPU of a 4-vCPU, 512 MiB guest under KVM, is at least halved by §19.9, with before and after numbers recorded
- [ ] in a 2-CPU, 512 MiB guest under TCG on both architectures, a 128 MiB file read three times, then a sequential read of a 1 GiB file, then the 128 MiB file again, refaults fewer than 10% of the 128 MiB file's pages, by the §19.10 refault counter
- [ ] RCU and every lock-free structure from §19.4 and §19.5 pass their loom models on the nightly job, and each model fails its weakened variant (§10.8)
- [ ] tag `phase-19` and cut the next release

### 19.1 Tracing
- [ ] static tracepoints beyond §10.7's set: network transmit and receive, TCP state changes, futex wait and wake, reclaim and writeback, and signal delivery
- [ ] the §10.7 per-CPU flight-recorder ring used as the trace buffer, with a reader that drains it live while writers run and a per-CPU count of records overwritten before they were read; the reader is in the portable half with a loom model (§10.8) of it racing a writer that wraps past it; the live drain is a tracefs file, and tracefs is mounted at `/sys/kernel/tracing` mode 0700, owned by root, as Linux mounts it, since records carry kernel addresses, so `trace_marker` below is root's too
- [ ] dynamic enable and disable per tracepoint; the cost of a disabled tracepoint measured with the §19.3 syscall-latency microbenchmark against a build without tracepoints, and the number recorded
- [ ] a userspace tool that writes the live drain as §10.7's Chrome trace-event JSON, so a live trace opens in Perfetto as a core's does
- [ ] tracefs's `trace_marker`, so applications emit into the same timeline through Linux's interface
- [ ] timestamps globally monotonic: an in-guest test merges every CPU's records from a `-smp 4` run and finds no inversion between causally ordered events, such as an IPI's send and its receipt

### 19.2 Profiling
- [ ] PMU setup: cycles and instructions where the host provides them, and cache and branch misses where the CPU has them, which no free host offers a guest ([Funded goals](#funded-goals)); where CPUID leaf 0xA or `ID_AA64DFR0_EL1.PMUVer` reports no PMU, setup skips with a registered marker, so x86_64 under TCG and HVF still pass, and the §10.1 KVM leg's job summary records whether its runner's KVM offered an architectural PMU. On aarch64 under TCG with `-icount shift=0` in a 1-CPU guest, an in-guest test counts `SW_INCR` increments exactly and a user loop's retired instructions (at least a million) within 1%, with EL1 excluded by the event filter, since QEMU emulates `SW_INCR`, `CPU_CYCLES`, and, under precise icount, `INST_RETIRED`
- [ ] sampling on a counter overflow interrupt with a stack walk
- [ ] per-process and per-thread accounting
- [ ] a flamegraph pipeline from samples to output
- [ ] Linux's `perf_event_open` for counting and sampling, so unmodified `perf stat` and `perf record` from the §14.9 mirror run; `kernel.perf_event_paranoid` defaults to 2, Linux's default, so a user without `CAP_PERFMON` counts and samples only its own user-space code; `/proc/kallsyms` in Linux's format, which `perf report` reads, lists zero addresses to a reader without `CAP_SYSLOG`; an in-guest test as uid 1000 finds no kernel address in a `perf record` sample or in `/proc/kallsyms`, and cannot open `/dev/kmsg` or tracefs

### 19.3 Benchmarks
- [ ] microbenchmarks: syscall latency, context switch, page fault, allocation, lock acquire
- [ ] subsystem: file read and write throughput, network throughput and latency, process creation rate
- [ ] macro: kernel build time, boot time, a mixed interactive workload
- [ ] all of it on the §10.1 KVM leg, with history recorded per runner CPU model and a threshold per model that fails
- [ ] each benchmark records its median and coefficient of variation over at least 5 runs per job in the §10.9 history; a benchmark whose coefficient of variation exceeds 5% on a runner CPU model gets no threshold on that model
- [ ] each workload also runs in §10.3's `irqoff` build once per nightly run on the KVM leg, and the §10.9 history records per site its longest IF-off stretch and the 99th percentile, with no threshold

### 19.4 Scheduler
- [ ] DESIGN §7.8 records the choice between weighted fair queueing and a virtual-deadline scheduler, with the §19.3 numbers that decided it
- [ ] the chosen scheduler replaces round-robin
- [ ] priorities and nice values with real effect; lowering a nice value below 20 minus `RLIMIT_NICE` needs `CAP_SYS_NICE`, as setpriority(2) and getrlimit(2) document
- [ ] a real-time class with `FIFO` and `RR` policies, entered as Linux allows: a caller without `CAP_SYS_NICE` sets a real-time policy only at a priority within its `RLIMIT_RTPRIO`, which is 0 by default, so it gets `EPERM`; `RLIMIT_RTTIME` bounds the CPU time a real-time thread runs without blocking, with `SIGXCPU` at the soft limit and `SIGKILL` at the hard one; and real-time threads never take a whole CPU, since the other classes keep 50 ms of each second, through the mechanism the baseline release uses (Linux's real-time throttling, `kernel.sched_rt_runtime_us` 950000 of `kernel.sched_rt_period_us` 1000000, or the fair-class deadline server that replaces it), with its `/proc/sys` files, writable by root. In-guest as uid 1000: a busy loop that asks `sched_setscheduler` for `SCHED_FIFO` at 99 gets `EPERM`; with `RLIMIT_RTPRIO` raised to 99 by root, a `SCHED_FIFO` busy loop on every CPU of a 4-CPU guest for 10 s leaves a `SCHED_OTHER` process's virtio-blk read completing within 2 s and the shell answering the harness
- [ ] priority inheritance for the real-time class: `FUTEX_LOCK_PI`, `FUTEX_LOCK_PI2`, `FUTEX_TRYLOCK_PI`, `FUTEX_UNLOCK_PI`, `FUTEX_WAIT_REQUEUE_PI`, and `FUTEX_CMP_REQUEUE_PI` over §13.5's owner field for user mutexes, and the kernel `BlockingMutex`; an in-guest inversion test (low priority holds the lock, medium priority spins, high priority waits) completes within a bound; RCU priority boosting through the same code (DESIGN §2.12): a reader that has held the current grace period open for longer than a bound (Linux's default is 500 ms) runs at the priority of RCU's grace-period thread, which Linux's `rcutree.kthread_prio=` sets on the §10.2 command line, until its outermost exit; an in-guest test at `-smp 1` with `rcutree.kthread_prio=10` preempts a fair-class reader inside its section with a `FIFO` 5 thread that polls for the end of a grace period started after the preemption, and a `kernel_tests` counter shows the reader boosted, back at its own priority after its exit, and the grace period ended
- [ ] load balancing: periodic first, then work stealing, measured against the periodic baseline before keeping it
- [ ] topology awareness from CPUID on x86_64 and MPIDR plus the device tree on aarch64: prefer a sibling core, keep a thread near its cache
- [ ] group scheduling with CPU shares and quotas: the scheduler half of §21.5's cgroup v2 `cpu.weight` and `cpu.max`
- [ ] wakeup latency under load reported at the 50th, 99th, and 99.9th percentiles, never as a mean; the exit gate bounds the 99th
- [ ] DESIGN §7.8 records the choice between per-CPU TCB ownership and a sharded TCB table, with the §19.5 contention numbers that decided it
- [ ] the chosen TCB layout replaces the global TCB table
- [ ] timeouts kept per CPU in a timing wheel that replaces `sched::TimeoutQueue`
- [ ] a lock-free structure the scheduler adds, such as a work-stealing deque or a cross-CPU wake list, lands with a loom model (§10.8) and a weakened variant the model rejects, as §19.5's queues do

### 19.5 Scalability
- [ ] RCU for read-mostly structures, as DESIGN §2.12 defines it: the dentry cache, the routing table, the mount table; their objects are pinned with get-unless-zero and freed a grace period after their count reaches zero
- [ ] DESIGN §7.7 records, for the TCP and UDP lookup tables, the neighbour table, and the interface list, whether each moves to RCU, with the numbers from this subsection's contention box, taken under §19.3's network throughput and latency benchmarks, that decided it; each that moves pins its entries with get-unless-zero (DESIGN §2.12), and a loom model of a lookup racing the entry's removal, whose weakened variant frees at the last put without a grace period, must fail
- [ ] RCU's read side, `synchronize`, and callback processing in the portable half, with loom models (§10.8) of a reader preempted across a grace period, a reader switched out in its section that leaves the blocked-reader list on another CPU (DESIGN §2.12), a CPU entering tickless idle (§19.6) mid-grace-period, and a CPU going offline (§19.6) with callbacks queued; each model must reject a weakened variant (§10.8) that ends a grace period before a pre-existing reader exits. Two more models, each rejecting its variant: get-unless-zero racing the last put and the deferred free, whose variant frees at the last put without a grace period; and a boosted reader's outermost exit racing its deboost, whose variant leaves the reader boosted
- [ ] DESIGN §2.12's read-side rules checked: §10.3's may-sleep assertion (DESIGN §2.9 rule 4) also requires the thread's read-side count to be zero, and an in-guest test that takes a `BlockingMutex` inside a read-side section under `arch::catch::catch_panic` finds the assertion; §12.6's allocator reads the count at entry as direct reclaim's fourth condition (DESIGN §4.4 rule 1), and an in-guest test that allocates inside a read-side section with memory exhausted draws on the reserve down to R/2 (DESIGN §4.4's atomic class) and then gets an error, with a `kernel_tests` counter showing that no reclaim ran; before the OOM killer runs, direct reclaim waits for the grace period in progress and its callbacks (DESIGN §4.4 rule 3), and an in-guest test that fills memory, queues the free of 64 MiB of heap objects behind a grace period through a `kernel_tests` hook, and then allocates 32 MiB succeeds with no OOM kill
- [ ] seqlocks where readers dominate and writers are rare, all through the one seqlock type the §10.8 loom model covers
- [ ] per-CPU counters aggregated on read, instead of a shared atomic on the hot path
- [ ] lock-free queues on the paths that need them, each with a loom model (§10.8), a weakened variant (one ordering weakened) that the model rejects, and a litmus test of its publish step in §11.7's `tests/litmus/`; DESIGN links each queue to its model instead of carrying a prose ordering argument
- [ ] the global locks split by hash or by CPU where profiling says it matters
- [ ] interrupt affinity rebalancing on the §6.3 table: move MSI-X and I/O APIC destinations (GIC SPIs and LPIs on aarch64) off a saturated core, measured before keeping it
- [ ] contention measured before and after each change, with the numbers recorded
- [ ] log records staged in a per-CPU buffer that a printer thread drains to serial, in place of the global IRQ-safe log ring, its TAS lock, and the serial try-lock sink, so an emitting CPU does not wait on the UART; contention measured before and after; this closes §5.5's deferred box
- [ ] page-cache lookups taken off the per-mapping lock where the contention measurement above finds a hot mapping (§12.5 already gives each block device and each file its own): lock-free radix-tree reads under the RCU read side from this subsection's first box, as Linux's XArray does, or the lock split by index range, as the measurement picks; DESIGN §10.6 records the choice
- [ ] VFS lookup off the global VFS lock: path walk reads the dentry cache and the mount table under the RCU read side from this subsection's first box, and falls back to a reference walk as DESIGN §2.12 says. The fallback's pin-and-recheck step is in the portable half with a loom model in which a rename moves the parent between the walk's check and its pin, whose weakened variant pins without rechecking the sequence count; an in-guest test at `-smp 4` runs 100,000 `stat` calls on a 16-component path while another CPU renames a sibling of each component back and forth, and every call succeeds. The contention numbers before and after, the share of walks that fell back, and the 99th-percentile grace-period length under the §19.4 wakeup-latency load are recorded in DESIGN §7.7; a 99th percentile above §19.4's boosting bound reopens DESIGN §2.12's boosting

### 19.6 Power and idle
- [ ] tickless idle: an idle CPU arms its next timer deadline instead of taking a periodic tick; monotonic time comes from the TSC or the generic counter, never from a count of timer interrupts, since that count stops while the tick is off (F027)
- [ ] idle through `mwait` on x86_64 where CPUID reports MONITOR, falling back to `hlt`, and `wfi` on aarch64, all at the shallowest state, which needs no firmware tables; deeper C-states and frequency scaling from ACPI are §20.2's, and measured power needs a physical machine ([Funded goals](#funded-goals))
- [ ] interrupt coalescing on the network and storage paths
- [ ] timer slack, per thread as on Linux (50 µs by default, inherited by a child), so unrelated wakeups can batch; the per-thread value §23.1's `PR_SET_TIMERSLACK` sets is built by whichever of §19.6 and §23.1 lands first
- [ ] CPU offlining for power management: migrate threads, redirect interrupts, park the core (PSCI `CPU_OFF` on aarch64); the reverse of bring-up, and not hotplug
- [ ] x86_64: `apic_init::send_ipi` and `send_ipi_all_ex_self` hold an `InterruptGuard` across the delivery-pending poll and the `ICR_HIGH`/`ICR_LOW` write pair, not across `busy_wait_ms`, so an IPI sent from an interrupt handler between the two writes cannot retarget the INIT or SIPI that onlining a parked core sends with the scheduler live; an in-guest test offlines and onlines a CPU while every other CPU sends IPIs (F112)

### 19.7 NUMA
- [ ] node topology and distances from SRAT and SLIT, tested under QEMU `-numa` on x86_64; aarch64 takes the same code path from ACPI in §20.7
- [ ] per-node buddy allocators with node-local allocation as the default
- [ ] NUMA-aware scheduling, keeping a thread near its memory
- [ ] page migration on persistent remote access
- [ ] per-node statistics, because the failure mode is invisible without them

### 19.8 I/O
- [ ] an async submission interface that is Linux's `io_uring`: `io_uring_setup`, `io_uring_enter`, and `io_uring_register` with Linux's SQE and CQE layouts and `mmap` offsets; opcodes added as callers need them, starting with `READ`, `WRITE`, `READV`, `WRITEV`, `FSYNC`, `OPENAT`, `CLOSE`, `STATX`, `ACCEPT`, `CONNECT`, `SEND`, `RECV`, `POLL_ADD`, and `TIMEOUT`
- [ ] `IORING_REGISTER_RESTRICTIONS`, `IORING_SETUP_R_DISABLED`, and the `io_uring_disabled` and `io_uring_group` files under `/proc/sys/kernel`, writable by root, as Linux defines them; a `seccomp` filter never sees an opcode, so a sandbox that must stop ring operations denies `io_uring_setup`, as on Linux
- [ ] liburing's `test/` suite, pinned, passes in-guest on both architectures with a checked-in skip list whose entries each name the missing opcode or a deliberate gap
- [ ] zero-copy paths for network send and file read
- [ ] `sendfile` and `splice`
- [ ] direct I/O bypassing the page cache
- [ ] user pages held by a device (direct I/O, the zero-copy send path, registered `io_uring` buffers) are pinned, with a pin count in the §12.1 unit's head `Frame`. A pinned anonymous page is always exclusive to one address space: pinning a COW-shared page breaks the share first, and `fork` copies a pinned page eagerly instead of sharing it, so the §12.3 write fault reuses a pinned page instead of copying it. Reclaim and §19.7 migration skip pinned pages
- [ ] in-guest: start a direct I/O read into part of a private page, then write elsewhere in that page while the read is in flight, once after a `fork` and once after an `mprotect` to read-only and back; the parent sees both the read's data and its own write, and the child's copy has neither
- [ ] polled I/O for virtio-blk, where the interrupt costs more than the spin, measured against interrupts; §20.4 applies the same to NVMe
- [ ] boot time reduced by parallelizing device probing and deferring what can be deferred
- [ ] virtio-blk zero-copy: DMA directly from page-cache pages once §12.5 unifies the caches; drop the bounce path
- [ ] packed virtqueues, measured against split before keeping them

### 19.9 Slab allocator
Moved from the memory phase: nothing before this phase needs it, and its gate is a contention measurement.

- [ ] object caches with a constructor, sized for a specific type
- [ ] slabs of one or more pages carved into objects, with a freelist in the unused object space
- [ ] per-CPU magazines so the common path takes no global lock
- [ ] caches for the hot types: TCB, process, inode, dentry, file, network buffer, request
- [ ] shrinking under memory pressure, driven by the same reclaim path as the page cache (§12.6, §19.10)
- [ ] `slabinfo` in the shell: per cache, objects active and total, pages used
- [ ] the general heap, §12.6's TLSF allocator, stays for odd-sized allocations; slab is not a replacement for it

### 19.10 Memory reclaim
Moved from the memory phase. §12.6 reclaims on demand, which is correct. This makes it fast and fair.

- [ ] watermarks (min, low, high), with min at §12.6's reserve R and low and high above it, derived as Linux derives them from `min_free_kbytes` and `watermark_scale_factor`, and a background reclaim thread, so allocations rarely stall in direct reclaim; the thread follows DESIGN §4.4's reclaim rules (it frees clean pages and leaves writing to the writeback and swap-out threads) and is a no-reclaim, progress-class thread itself (DESIGN §4.4)
- [ ] the §12.5 LRU split into an active and an inactive list, so a single sequential scan does not evict the working set; counters for promotions, demotions, and refaults
- [ ] a histogram of regions visited per §12.1 reverse-map walk, from §19.7's migration, swap when enabled, and a `kernel_tests` walk hook, recorded on the §17.5 build loop and on a fork fan-out test in which one parent keeps 1,000 live children that each write to their copy of one page and the hook walks those copies; if the 99th percentile passes 64 regions on either, anonymous objects become Linux's chained design (`anon_vma_chain`), with DESIGN §4.6 changed in the same commit
- [ ] the §12.6 OOM score gains Linux's per-process adjustment, set through `/proc/<pid>/oom_score_adj`, a file built by whichever of §19.10 and §23.4 lands first, and added as Linux's `oom_badness` adds it (the adjustment times the scope's RAM plus swap, over 1000), with -1000 excluding the process; the score has no nice term, as Linux's has had none since 2.6.36; an in-guest test gives two processes of equal size different adjustments and checks their order in §12.6's logged score table, and a process at -1000 is never chosen
- [ ] allocation latency under pressure measured before and after, with the numbers recorded, and beside them each §12.6 reserve class's low-water mark from the same runs and from the Phase 15 flood; a progress-class low-water mark of zero, or an atomic class held at R/2 by traffic within §15.1's limits, reopens R's sizing in DESIGN §4.4; the same runs record `meminfo`'s count of OOM victims that reached §12.6's reaper deadline unreaped

---

## Phase 20: Hardware Models

**Goal.** Drivers for the devices real machines carry, proven on QEMU's models of those devices and on
real machines' firmware tables, and kept working by a nightly job. No physical machine is assumed; the
machines that would close the same lines on silicon are in [Funded goals](#funded-goals).

**Unlocks.** USB, NVMe, AHCI, Intel NICs, SD and eMMC, HDA, and watchdogs, each driver tested against a
QEMU model. An AML interpreter checked against 815 real machines' tables, and aarch64 booted through ACPI
(§20.7). S3 (§20.2), x2APIC past 255 CPUs (§20.1), and PCIe hotplug and UEFI variables (§20.9), which
§22.2's unattended updates and the later eras build on. The §20.8 nightly job on device models and the
§20.1 model list, which later phases add their profiles and records to; a machine from Funded goals
fills `docs/HARDWARE.md`'s physical section.

**Architectures.** Both, on QEMU. §20.1 is x86_64's platform: `q35` and `pc` under OVMF and SeaBIOS.
§20.7 is aarch64's: `virt` with a device tree or with ACPI under the aarch64 edk2 build, and `sbsa-ref`
under TF-A and edk2. The §20.2 interpreter and the §20.3 to §20.6 drivers build for both, and §20.7 runs
the ones its machines carry. S3 is x86_64 only: `virt` has no `_Sx` objects and QEMU's PSCI has no
`SYSTEM_SUSPEND`, so an aarch64 guest suspends only to idle. The i2c and SMBus box is x86_64 only, since
neither aarch64 machine has an I2C controller. `sbsa-ref` runs under TCG only, because its firmware runs
at EL3. x86_64 profiles run on the hosted x86_64 runner under KVM and under TCG; aarch64 profiles run
under TCG on the hosted arm64 runner, which has no KVM, and under HVF only as §10.9 dev-host records.
§20.9 is gated under QEMU on both, ACPI hotplug on `q35` only.

**Exit gate**
- [ ] a disk image holding the ISO, attached as QEMU's `usb-storage` behind `qemu-xhci` with no other boot disk, boots under SeaBIOS and OVMF on `q35` and under the aarch64 edk2 build on `virt`, and a second `usb-storage` disk passes Phase 7's pattern and concurrent read-write tests through §20.3's mass-storage driver (TCG, 2 vCPUs, 1 GiB)
- [ ] storage, on `q35` and on aarch64 `virt` (TCG, 2 vCPUs, 1 GiB): Phase 7's pattern and concurrent read-write tests pass on NVMe, on an SD card and an eMMC behind `sdhci-pci`, and on `q35`'s built-in AHCI
- [ ] root, in the same guests: root mounts from NVMe on both machines and from AHCI on `q35`
- [ ] network, in the same guests: a TCP client fetches 1 GiB from the host with no corruption through vibeOS's e1000e driver, and again through its igb driver
- [ ] input, in the same guests: `evtest` from the §14.9 mirror reads, from its `/dev/input/event<N>` node, a key the harness sends with QMP `input-send-event` to a `usb-kbd` behind `qemu-xhci`
- [ ] aarch64 reaches `shell ready` through ACPI with no device tree, on `virt` with `acpi=on` under the aarch64 edk2 build and on `sbsa-ref` under its pinned firmware (TCG, 4 vCPUs, 2 GiB), taking CPUs, the GIC, and the ITS from the MADT, the timer from GTDT, the console from SPCR, PCIe from MCFG, and the PSCI conduit from the FADT (HVC on `virt`, SMC on `sbsa-ref`) (§20.7)
- [ ] `make test-kernel` passes on both of those ACPI boots
- [ ] `sbsa-ref`, in that ACPI boot, mounts root from its built-in AHCI, fetches the 1 GiB above through its e1000e, and reads the harness's key through its built-in xHCI
- [ ] on ACPI `virt` with `iommu=smmuv3`, the virtio-blk and virtio-net in-guest tests pass through the SMMUv3 that IORT names
- [ ] a `-numa` variant of ACPI `virt` reports SRAT's nodes and SLIT's distances
- [ ] in host tests, the AML interpreter loads the DSDT and SSDTs of every machine variant in QEMU's `tests/data/acpi` at the pinned QEMU release, and of at least 95% of the machines in the pinned linuxhw/ACPI snapshot whose tables the pinned `iasl` recompiles, and evaluates every `_PRT` it finds (§20.2)
- [ ] at least 95% of the tests in ACPICA's aslts functional, complex, and exceptions collections pass under the interpreter
- [ ] every MADT, FADT, HPET, MCFG, DMAR, IVRS, SRAT, SLIT, GTDT, IORT, and SPCR in both corpora goes through its §2.4, §18.1, §19.7, or §20.7 parser without a panic, and each one refused is logged with its reason
- [ ] each failure in the three lines above is on a checked-in list naming the machine or test and a reason, and the runner fails on a listed case that passes, so the lists only shrink
- [ ] clean shutdown through ACPI S5 on `q35` and `pc`; reboot through the FADT reset register on `q35` and through the 8042 on `pc`, whose revision-1 FADT has no reset register; both through PSCI on `virt` and `sbsa-ref`. Each is seen as QEMU's exit or its QMP `SHUTDOWN` or `RESET` event, after a registered marker naming the path taken: the PM1 control block written with `_S5`'s `SLP_TYPa`, the FADT reset register's address space, the 8042, or the PSCI function (F097)
- [ ] QMP `system_powerdown` shuts the guest down cleanly through the ACPI power button on `q35` and on `virt` booted with ACPI
- [ ] 100 S3 suspend and resume cycles on `q35` under OVMF, on the nightly job (TCG, 2 vCPUs, 1 GiB): each cycle enters S3 through `_S3`, with QMP reporting `SUSPEND`, and resumes through the FACS waking vector, woken alternately by QMP `system_wakeup` and by an RTC alarm; the NVMe disk, the e1000e NIC, and the `usb-kbd` work after the last cycle
- [ ] OVMF on `q35` with 288 vCPUs (TCG, `-cpu max`, 4 GiB) hands off in x2APIC mode, and every CPU reaches `smp: done`; with `intel-iommu,intremap=on,eim=on`, a virtio device's MSI-X interrupt is delivered to the CPU with the highest APIC ID, and without an IOMMU no device interrupt targets an APIC ID above 254 (§20.1)
- [ ] every §20.6 driver passes its in-guest test on each §20.8 profile that carries its device
- [ ] under QEMU on both architectures (TCG, 2 vCPUs, 1 GiB), a virtio-blk disk added with `device_add` on a PCIe root port during a write workload appears under its persistent name, and one removed with `device_del` fails its in-flight I/O without a panic, through native hotplug (the §20.9 `q35` variant and aarch64 `virt`) and through ACPI hotplug on `q35`; a variable written through efivarfs reads back after a reboot under OVMF and the aarch64 edk2 build
- [ ] the §20.8 nightly job is green at the gated commit and on each of the six nights before it, and §10.9's CI history holds a record from every §20.8 profile for each of those nights
- [ ] `make test` passes on vibeOS, itself a 4-vCPU, 4 GiB guest under KVM on the hosted x86_64 runner, on the nightly job and on the terms of the Phase 17 gate; the aarch64 run under HVF on the dev host is a §10.9 record at the gated commit
- [ ] tag `phase-20` and cut the next release

### 20.1 x86_64 platform models
- [ ] both boot paths on each model firmware: OVMF (UEFI) and SeaBIOS (BIOS) on `q35` and `pc`, each booting the ISO from CD, NVMe, and `usb-storage`, chosen by `bootindex`
- [ ] NX checked before use: `paging_init::install` reads CPUID.80000001H:EDX[20] before it sets `EFER.NXE`; on a CPU without NX, boot halts before `mov cr3` with a registered marker naming the missing feature, instead of triple-faulting on a `wrmsr` that runs before `arch::idt::init` loads the kernel's IDT; an e2e boot with `-cpu qemu64,-nx` expects that marker (F090)
- [ ] CPU microcode updates for Intel and AMD from the vendors' published files, pinned by hash with their licenses recorded, fetched at test time, and loaded from a test image's initrd, since §14.10's policy publishes no proprietary binary: applied on the BSP before the §18.3 speculation mitigations read CPUID, and on each AP in its bring-up path; the container parsing and CPU-signature matching in the portable half, host-tested against the published files. In-guest, `-cpu Skylake-Server` and `-cpu EPYC-Milan` under TCG, each with its `ucode-rev` property set below the published update's revision, present signatures the published files cover; the loader selects the matching update, performs the load, and logs each CPU's revision before and after. QEMU accepts the load and leaves the revision unchanged, so a revision that changes is a Funded goals line
- [ ] x2APIC: read `IA32_APIC_BASE.EXTD` at LAPIC enable. When firmware hands off in x2APIC mode, which may be locked, drive the LAPIC and ICR through MSRs, never clear EXTD, and never map the xAPIC MMIO page. Parse MADT types 9 and 10 alongside 0 and 4. QEMU exercises the handoff by booting OVMF on the §18.1 `q35` configuration (`pc` caps at 255 vCPUs) with more than 255 vCPUs, which hands off in x2APIC mode, under KVM or under TCG from QEMU 9.0, the first release whose TCG models x2APIC. The ICR write orders earlier stores as DESIGN §7.6 says: `apic_init::send_ipi` runs the steps of a portable `apic::ipi_send_plan`, a step list like `tsc_deadline_arm_plan`, which puts `mfence` then `lfence` before every x2APIC ICR `WRMSR`, INIT and SIPI included, and a compiler barrier before every xAPIC ICR low write; a host test checks both plans, and the call-site fences that remain in `smp_init::start_one` and `ipi_init` go in the same commit
- [ ] `apic::apic_base_msr` keeps EXTD set when the value it reads has EXTD set, since writing EN=1 with EXTD=0 while EXTD is set is the x2APIC-to-xAPIC transition, which raises `#GP`; its host test covers an input with EXTD=1 (F028)
- [ ] no CPU cap below the x2APIC range: `MAX_CPUS` and the `u8` APIC IDs in `src/acpi.rs`, and the 64-bit online, waiter, and shootdown masks behind `MAX_IPI_CPUS` in `src/ipi.rs`, replaced by tables sized from the MADT with 32-bit APIC IDs and CPU masks sized from the CPU count, so the boot above with more than 255 vCPUs brings every CPU online; today a CPU past the 64th is dropped from the MADT table without a log line
- [ ] an AP that misses the 3 s ready timeout is sent INIT, its online bit is cleared, and its stack, GDT and TSS, IST stacks, `PerCpu` slot, and idle TCB are leaked, not freed by `smp_init::free_ap_resources`, since an AP that accepted a SIPI may still run on them. An in-guest test stalls one AP past the timeout through a test hook in `ap_entry` before it stores `ready`; the next AP comes up, and the frame count shows only the stalled AP's resources missing; DESIGN §7.4 step 6 and §9.5 updated in the same commit (F032)
- [ ] `apic_init::disarm_timer` writes `IA32_TSC_DEADLINE` only in `TimerMode::TscDeadline`, since MSR `0x6E0` exists only when CPUID.01H:ECX[24] is set and `prove()`'s periodic-mode failure path would otherwise take `#GP` instead of falling back to the PIT; the per-mode disarm sequence is in `vibeos-core` with a host test per `TimerMode` (F092)
- [ ] PCI enumeration and the §6.1 device registry sized from what the scan finds: `MAX_SCAN` in `src/pci.rs` and `MAX_DEVICES` in `src/dev.rs` gone, since server boards and §20.9's hotplug root ports pass 64 functions; today the 65th is dropped without a log line. A `q35` line with 72 `pcie-root-port` functions, eight per slot, enumerates every one
- [ ] an ECAM configuration-page miss after `pci_init`'s 64-entry cache is full reuses or unmaps its `ioremap` VA instead of leaking one page of the window per miss; the 72-function `q35` boot above, run with `-vga none` so the MCFG window lies above `map_end`, ends its scan with at most 64 ECAM pages in the window (F115)
- [ ] ECAM addressed from bus 0: `pci::ecam_phys` returns `base + (bus << 20 | dev << 15 | fn << 12 | off)` and keeps the `[start_bus, end_bus]` check, since the MCFG base corresponds to bus 0 (PCI Firmware Spec 3.2 §4.1.2); `acpi::parse_mcfg` returns every allocation keyed by segment and bus range; host tests cover a nonzero `start_bus` and the multi-entry MCFGs in the linuxhw/ACPI snapshot, and the `pci.rs` unit test that asserts the relative formula is changed to assert the absolute one (F045)
- [ ] configuration mechanism #1 on every bus: where ECAM does not cover a bus, `HwCfg::read32` and `write32` use `0xCF8`/`0xCFC` for offsets below `0x100` on any bus number, instead of returning `0xFFFF_FFFF` and dropping writes past bus 0; on `pc`, which has no MCFG, a virtio-blk behind a `pci-bridge` enumerates, binds, and passes Phase 7's pattern test (F114)
- [ ] BAR sizing as PCI Local Bus Spec 3.0 §6.2.5.1 gives it: `pci::probe_bar` saves `COMMAND` and clears its IO and MEM decode bits, sizes every BAR of the function under one `CFG_LOCK` hold, and then restores the BARs and `COMMAND`; `size_from_mask` ORs `0xFFFF_0000` into an I/O mask whose bits 31:16 read zero, host-tested with `decode_bar(0xC001, 0, 0x0000_FFE1, 0)` returning 32 bytes (F113)
- [ ] PCI capability walks mask every capability pointer with `0xFC`, refuse a capability whose fields would reach past `0x100` (`0x1000` for an extended capability under ECAM), and read dwords only at 4-byte-aligned offsets, composing others from `read8` and `read16`, so no ECAM access is an unaligned `read_volatile`; `pci::walk_caps`, `virtio::read_modern_caps`, and `pci::read_msix_cap` are host-tested over a fake configuration space with a pointer whose low bits are set and a vendor capability at `0xF0` (F118)
- [ ] BARs above 32 MiB mapped, with the `ioremap` window sized at boot from the BAR total instead of fixed at 256 MiB (DESIGN §4.1), since real GPUs and 100GbE NICs expose larger BARs; tested with an NVMe model whose 512 MiB controller memory buffer (`cmb_size_mb=512`) needs a BAR larger than today's window
- [ ] I/O APIC and MSI routes to APIC IDs above 254 through §18.1's interrupt remapping, with its entries in x2APIC format (VT-d's extended interrupt mode, AMD-Vi's `XTEn`), tested on the §18.1 `q35` configuration with `eim=on` or `xtsup=on`; on a machine without a usable IOMMU, the §6.3 affinity API keeps device interrupts on CPUs whose APIC ID is 254 or lower
- [ ] the memory map as each firmware builds it, never assumed to be QEMU's default layout: OVMF and SeaBIOS on `q35` and `pc` with 3, 4, and 5 GiB and with a size that is not a multiple of 2 MiB, and `q35` with `-numa` nodes whose ranges leave holes, each boot to `shell ready` and pass the Phase 1 memory tests
- [ ] the kernel PML4 allocated below 4 GiB, since the AP trampoline loads a 32-bit CR3 and `smp_init::start_one` skips every AP, printing `smp: cr3 above 4GiB`, when it lies above; the 5 GiB boots of the box above run with `-smp 4` and print `smp: ap online` for every AP (kernel review invariant I16)
- [ ] no physmap large page spans two MTRR memory types, which the SDM leaves undefined outside the fixed-range first MiB: at boot, when CPUID reports MTRRs, the kernel reads `IA32_MTRRCAP`, `IA32_MTRR_DEF_TYPE`, and the variable MTRR pairs, and before `mov cr3` maps with 4 KiB pages any 2 MiB physmap block whose ranges differ in type. The decision is in the portable half, host-tested against synthetic layouts that put a type boundary inside a 2 MiB block, and the boot log counts the blocks split under OVMF and SeaBIOS; DESIGN §4.3 and §9.2 updated in the same commit
- [ ] firmware tables beyond QEMU's: the §2.4 parsers (MADT, FADT, HPET, MCFG) and §18.1's DMAR and IVRS parser host-tested against every data table in the linuxhw/ACPI snapshot, whose `iasl` dumps end each data table with its raw bytes, and against QEMU's `tests/data/acpi`; each vendor quirk found is handled or rejected with a logged reason, and listed in `docs/HARDWARE.md` with the machines it came from
- [ ] `acpi::parse_madt` drops a repeated enabled LAPIC or x2APIC ID with one logged warning, and `smp_init` never sends INIT to an APIC ID already assigned a `PerCpu`, so a duplicate cannot reset a running AP or leave a CPU in the online mask that no core answers for; host tests feed a MADT that lists one ID twice (F035)
- [ ] an I/O APIC whose VER register reads `0xFFFF_FFFF` is logged and skipped as absent instead of registered with `max_index` 255, and `apic::ioapic_max_index` clamps to 119, so a phantom listed first cannot shadow a present I/O APIC's GSIs in `find_ioapic`; host tests cover `0xFFFF_FFFF` and a valid VER value (F095)
- [ ] `HhdmPhys` and §11.2's on-demand firmware-table leaves refuse a physical address at or above MAXPHYADDR, and a table whose header length exceeds 1 MiB is refused before `checksum_range` maps it; the bounds are in `vibeos-core` with host tests (F136)
- [ ] 8259 presence decided once, from the MADT `PCAT_COMPAT` flag, with a mask-register read-back probe when the flag is clear, as Linux does; it replaces both the FADT `LEGACY_DEVICES` skip before `lidt` and the unconditional second ICW sequence (§2.3). On a machine with no 8259, the PIT fallback through LINT0 ExtINT halts with a named reason. `FadtInfo::legacy_8259` is deleted, and so is the `legacy_8259` assertion in `acpi.rs`'s `fadt_iapc_and_reset` test, which keeps its reset and sleep-control assertions. The decision is in `vibeos-core`, host-tested with `PCAT_COMPAT=1, LEGACY_DEVICES=0` and `PCAT_COMPAT=0` tables and against the snapshot's MADTs; DESIGN §3.3, §5.5, and §7.1 updated in the same commit (F094)
- [ ] external NMIs delivered: the BSP programs LINT1 in NMI delivery mode with the pin, polarity, and trigger from the MADT's Local APIC NMI entries (types 4 and `0x0A`), APs keep LINT1 masked as Linux's `setup_local_APIC` does, and MADT NMI Source entries (type 3) are routed at the I/O APIC; under TCG on `pc` and `q35`, the harness sends QMP `inject-nmi` and finds the NMI handler's registered marker from CPU 0; host tests cover the MADT NMI entries; DESIGN §2.5 updated in the same commit (F096)
- [ ] a console without a 16550: QEMU's `usb-serial`, an FTDI FT232BM model, behind xHCI as Linux's `/dev/ttyUSB<N>`, which replays the §5.5 log ring when it registers, so a boot with `-serial none` gives the harness every marker from the `usb-serial` chardev; the framebuffer console carries output from the first line
- [ ] a panic record, the §5.6 dump and the last log records, kept with a header and checksum in a RAM region at a physical address fixed by a §10.2 command-line option, which the frame allocator never hands out; at boot the region is checked against the Limine memory map and module placement, a conflict is logged rather than trusted, and a region whose header or checksum fails reads as empty. It is read back and logged on the next boot after a warm reset, since there is no host to catch it; tested under QEMU with a deliberate panic and the monitor's `system_reset` on both architectures. Firmware-backed stores and full memory dumps are Phase 25
- [ ] the model list: `docs/HARDWARE.md` names each §20.8 profile with its machine type, the accelerators it runs under, its device models, firmware builds and their hashes, and QEMU version, generated from the harness profiles, with the §20.2 corpus results per machine, the linuxhw/ACPI attribution its CC-BY-4.0 license requires, and a physical section that stays empty until a Funded goal fills it; `scripts/check_hardware.py` in `make check` fails when the generated part differs from the profiles

### 20.2 ACPI runtime
- [ ] an AML interpreter that loads a DSDT and its SSDTs into one namespace and evaluates control methods, done when it passes the exit gate's corpus lines
- [ ] the interpreter in the portable half, fuzzed like every parser, and host-tested against three free corpora, none vendored: QEMU's `tests/data/acpi` (GPL-2.0, fetched at the pinned QEMU release), ACPICA's aslts compiled by the pinned `iasl`, and the linuxhw/ACPI snapshot of 815 machines at the commit §18.1 pins (CC-BY-4.0), whose DSDTs and SSDTs are `iasl`-decoded text that the pinned `iasl` recompiles, so the AML is equivalent to the machine's but not byte-identical
- [ ] the device tree from the DSDT and SSDTs, resource assignment, `_CRS` parsing, `_PRT` interrupt routing
- [ ] power management: S5 shutdown, and S3 suspend with resume through the FACS waking vector; S4 hibernation, which needs §12.7's swap, is §31.8's stretch
- [ ] shutdown and reset from the FADT, not QEMU constants: `acpi::parse_fadt` reads Flags (offset 112) with `RESET_REG_SUP` and `HW_REDUCED_ACPI`, `PM1a_CNT_BLK`, `PM1b_CNT_BLK`, and their `X_` forms; `SLP_TYPx` comes from the interpreter's `_S5` package; `RESET_REG` is written, 8 bits wide, only when `RESET_REG_SUP` is set, in the I/O, memory (mapped UC through `ioremap` at boot), or PCI configuration space its address space ID names; the writes to ports `0x604` and `0xB004` and the fixed `SLP_TYP` of 5 in `shell_init.rs` are deleted; host tests parse the corpus FADTs (F097)
- [ ] `suspend` and `resume` on the `Driver` trait (§6.1), called in DESIGN §12.2's per-device order, a device's children and consumers suspending before it and its parent and suppliers resuming before it, which S3 needs
- [ ] deeper C-states from `_CST` (`_LPI` on aarch64), with a governor choosing depth by predicted idle duration; P-states and frequency scaling from `_PSS` or CPPC (`_CPC`); the §19.6 idle path gains both. QEMU generates none of these objects, so they are host-tested against the corpus's, evaluated through the interpreter over simulated fixed hardware, and in-guest the governor finds no deeper state and keeps §19.6's shallowest one with a logged reason
- [ ] thermal zones and fan control, host-tested against the corpus's `_TZ` objects with a simulated embedded-controller address space, since QEMU models no thermal zone
- [ ] hotplug notifications and the power button: QMP `system_powerdown` reaches the guest as the fixed-feature power button on `q35` and through the Generic Event Device's `_EVT` on `virt` booted with ACPI; §31.1 delivers it as `KEY_POWER`
- [ ] `_OSI` answered as Linux answers it, true for each Windows version string Linux accepts and false for `Linux`, and the vendor quirks that come with it, each found in the corpus and listed in `docs/HARDWARE.md` with its machines

### 20.3 USB
- [ ] xHCI: controller init, command and event rings, device slots, endpoint contexts; tested on both of QEMU's controller models, `qemu-xhci` and `nec-usb-xhci`, with MSI-X and with pin interrupts (`msi=off,msix=off`), and on `sbsa-ref`'s built-in controller (§20.7)
- [ ] enumeration: address assignment, descriptor parsing, configuration selection, at the full, high, and SuperSpeed rates QEMU's device models present
- [ ] hub support, including nested hubs, tested with chained `usb-hub` devices; each USB device is a child of its hub in DESIGN §12's tree, so detaching a hub removes the devices behind it first
- [ ] HID: keyboard and mouse boot protocol, then report descriptor parsing, delivered through §16.4's input abstraction; tested on `usb-kbd`, `usb-mouse`, and the absolute `usb-tablet`
- [ ] mass storage over bulk-only transport, tested on `usb-storage` and on `usb-bot` with several LUNs
- [ ] USB serial: FTDI's protocol, as QEMU's `usb-serial` models an FT232BM, exposed as `/dev/ttyUSB<N>`; the other adapter chips are Funded goals
- [ ] removal: a USB device detached with `device_del` during a transfer fails its I/O, its nodes go away, and nothing panics

### 20.4 NVMe
Moved from Phase 7: nothing before this phase needs either driver. Both are tested on QEMU's models,
`nvme` and `ich9-ahci`; physical drives are a Funded goal.

- [ ] controller identify, admin queue setup, I/O queue creation per CPU
- [ ] submission and completion queue handling with doorbells and phase tags
- [ ] the §12.5 request timeout: a timed-out command is aborted with the admin Abort command, then the controller is reset (`CC.EN` cleared, `CSTS.RDY` read back as 0) when the abort does not complete, and I/O resumes on the reset controller; tested with a `kernel_tests` hook that withholds one completion
- [ ] namespace enumeration, tested with several `nvme-ns` namespaces on one controller, each as Linux's `/dev/nvme<N>n<M>`
- [ ] read and write commands, flush, dataset management for discard
- [ ] MSI-X per queue
- [ ] the fast path built for depth from the start, since NVMe's whole point is parallelism
- [ ] polled completion for the fast path, measured against interrupts as §19.8 did for virtio-blk, under KVM in a 4-vCPU, 1 GiB guest, with the numbers recorded

### 20.5 AHCI
- [ ] HBA and port initialization, command list and FIS structures
- [ ] identify device, LBA48 read and write
- [ ] the §12.5 request timeout: a timed-out command is recovered by a port reset (COMRESET), after which the port's other commands are reissued; tested with a `kernel_tests` hook that withholds one completion
- [ ] ATAPI detection so a CD-ROM does not look like a broken disk, tested with `ide-cd` on the controller
- [ ] tested on `q35`'s built-in ICH9 AHCI at 00:1f.2, on an added `ich9-ahci` on aarch64 `virt`, and on `sbsa-ref`'s built-in controller, which has no PCI function and binds from its ACPI description (`_HID` `LNRO001E` with the AHCI class code in `_CLS`)

### 20.6 Devices
QEMU models every device below on both architectures unless the box says otherwise. The parts it does
not model are Funded goals.

- [ ] NVMe and AHCI error handling: a read or write error injected with QEMU's `blkdebug` filter under the drive completes that request with an error, is counted per device, and leaves the device serving I/O
- [ ] SMART on the AHCI model (`SMART READ DATA`) and the NVMe health log, with a critical warning raised at runtime through QEMU's `smart_critical_warning` property logged; unmodified `nvme smart-log` from the §14.9 mirror reads the health log through Linux's `NVME_IOCTL_ADMIN_CMD` on `/dev/nvme<N>`
- [ ] Intel NICs beside §15.2's e1000: e1000e (82574L) and igb (82576), tested on QEMU's `e1000e` and `igb`; the Realtek 8168 family, which QEMU does not model, and NIC firmware loading are Funded goals
- [ ] USB Ethernet: the CDC-ECM class driver, tested on QEMU's `usb-net` in its CDC Ethernet configuration; CDC-NCM and the ASIX AX88179 family, which docks and adapters carry and QEMU does not model, are Funded goals
- [ ] Intel HDA: controller, codec discovery, and output streams, tested on `intel-hda` and `ich9-intel-hda` with `hda-output`: an in-guest test plays a 1 kHz tone into QEMU's `wav` audiodev, and a host check finds that frequency in the file; the rest of the audio stack is Phase 34
- [ ] `DmaAlloc::dma32` allocates from a buddy zone below 4 GiB, so the whole buffer lies below 4 GiB rather than only off a 4 GiB crossing, since the LIFO free lists pop high memory first; a host test allocates from a pool that spans 4 GiB, and an in-guest test in a 6 GiB guest (OVMF on `q35`, and `virt`) allocates 64 `dma32` buffers and finds each below 4 GiB; DESIGN §4.7 updated in the same commit (F030)
- [ ] SD and eMMC: an SDHCI driver with SD and eMMC card support, tested on `sdhci-pci` with `sd-card` and with `emmc` (QEMU 9.1 or later), each card as Linux's `/dev/mmcblk<N>`, and root on either; `sdhci-pci` reports no 64-bit system bus support, so the driver takes its SDMA and ADMA2 buffers from `dma32`, and the SD and eMMC tests also pass in the 6 GiB guest above (F030)
- [ ] i2c and SMBus, x86_64 only: the ICH9 SMBus controller on `q35` behind Linux's i2c-dev interface (`/dev/i2c-<N>` with `I2C_SLAVE` and `I2C_SMBUS`), so `i2cdetect` and `i2cget` from the §14.9 mirror find and read the eight EEPROMs QEMU puts at 0x50 to 0x57; sensors and embedded controllers are Funded goals
- [ ] watchdog timers: `i6300esb` on both architectures, the ICH9 TCO timer built into `q35`, and the SBSA generic watchdog built into `sbsa-ref`, found through GTDT, each armed, fed, and stopped by a kernel driver; an in-guest test stops feeding each, and the harness sees QEMU's `WATCHDOG` event and the reset; §22.2 arms one at boot and §25.5 puts Linux's `/dev/watchdog` over them

### 20.7 aarch64 platform models
No aarch64 server or board is assumed. `virt` and `sbsa-ref` stand in for their firmware and devices,
Linux's device trees stand in for boards', and the physical machines are Funded goals.

- [ ] the ACPI boot path on two machines: `virt` with `acpi=on` under the aarch64 edk2 build, a harness profile beside §11.7's `acpi=off` device-tree line, and `sbsa-ref`, whose firmware (TF-A and edk2-platforms' SbsaQemu) gives the OS ACPI only and runs at EL3, so under TCG only; its two flash images are pinned by SHA-256 and fetched, not vendored: the prebuilt pair QEMU's own `sbsa-ref` tests use, or the same pinned sources built by the job
- [ ] ACPI where firmware gives no device tree, as servers and `sbsa-ref` do: the §2.4 parser for the FADT's Arm boot-architecture flags (`PSCI_COMPLIANT`, `PSCI_USE_HVC`), which say whether PSCI is present and name its conduit where there is no `/psci` node, and for MADT, GTDT, SPCR, MCFG, IORT (the ITS device IDs behind each root complex, the §18.1 SMMUv3, and its RMR nodes, whose ranges are mapped identity into the domains of the stream IDs they name, as §18.1 maps RMRRs and IVMD blocks, with those stream IDs left in bypass from SMMU enable until their domain attaches; QEMU generates no RMR node, so those are host-tested against IORT tables the pinned `iasl` compiles), SRAT (§19.7's NUMA code path on aarch64, tested with `-numa` on `virt`), and TPM2, so §18.7's driver finds `tpm-tis-device` on the ACPI `virt` profile as it does on x86_64, and the §20.2 interpreter for the DSDT
- [ ] boards' device trees in host tests: Linux's trees for the Raspberry Pi 4 (`bcm2711-rpi-4-b`) and 5 (`bcm2712-rpi-5-b`) and for Apple's M1 to M3 SoCs, compiled by a pinned `dtc` at a pinned Linux tag, go through the §11.5 parser, which finds each tree's CPUs and enable method, interrupt controller, timer, and `serial0` console, and names each node it has no driver for, such as Apple's AIC and BCM2712's PCIe root complex, which is not ECAM, instead of failing. Trees licensed `GPL-2.0 OR MIT` or `GPL-2.0+ OR MIT` are vendored under MIT with their headers; GPL-2.0-only trees, such as `bcm2711-rpi-4-b`, are fetched by the test and never committed
- [ ] the GIC and ITS as the firmware describes them rather than as `virt` lays them out (§11.3): `sbsa-ref` puts its distributor, redistributors, and ITS at other addresses, found from its MADT
- [ ] the NICs for the Phase 20 gate: §20.6's igb and e1000e on `virt`, and `sbsa-ref`'s default e1000e
- [ ] NVMe root on `virt`, AHCI root on `sbsa-ref`, and SD or eMMC root through `sdhci-pci`, as a board boots
- [ ] USB over xHCI, shared with §20.3: `qemu-xhci` on `virt`, and `sbsa-ref`'s built-in controller, which has no PCI function and binds from its ACPI description (`_HID` `PNP0D10`), mapping its `_CRS` ranges through claims as a PCI driver maps its BARs (DESIGN §12.3)
- [ ] serial on the PL011 that SPCR names, on both machines, and the same nightly treatment as x86_64 in §20.8

### 20.8 Nightly hardware-model CI
Every CI machine is a GitHub-hosted runner: `ubuntu-26.04` on x86_64, with `/dev/kvm` once a udev rule
opens it to the runner user, and `ubuntu-26.04-arm`, which has no KVM. A job stops at 6 hours, and an
account runs 20 jobs at once. Hardware performance events and physical machines are Funded goals.

- [ ] a `hardware-models` workflow on the nightly schedule, with the `workflow_dispatch` trigger §10.9 requires: one job per harness profile and accelerator, the profiles named in `docs/HARDWARE.md` (`q35`, `pc`, `virt` with a device tree, `virt` with ACPI, and `sbsa-ref`), each a command line for the §10.1 pinned QEMU that attaches every device model this phase drives that the machine can take: NVMe with several namespaces, AHCI, xHCI with `usb-kbd`, `usb-mouse`, `usb-tablet`, `usb-storage`, `usb-net`, and `usb-serial`, e1000e, igb, HDA, `sdhci-pci` with SD and eMMC, the machine's watchdogs, and `pcie-root-port`s, plus §18.1's IOMMU on every machine but `pc`, which takes none (`intel-iommu` or `amd-iommu` on `q35` as §18.1 runs them, `iommu=smmuv3` on both `virt` profiles, and `sbsa-ref`'s built-in SMMUv3), and §18.7's swtpm TPM on every machine but `sbsa-ref`, which takes none; the x86_64 profiles run under KVM and under TCG, the aarch64 ones under TCG
- [ ] each job runs the in-guest tiers and this phase's model tests on its profile and commits a record (profile, accelerator, runner CPU, QEMU version, firmware hashes, result, and on an x86_64 KVM job the nesting its runner offers a guest: `vmx`, `svm`, or `none`) to §10.9's `ci-history`; the nesting is read on the host before the boot, with `kvm_intel` or `kvm_amd` loaded with `nested=1`, from the `vmx` and `svm` properties of a `-cpu host` vCPU (QMP `qom-get` on a QEMU started with `-S`), so it does not rest on vibeOS's own VMX or SVM code; `scripts/ci_history.py` fails when a night lacks a profile's record, since GitHub delays scheduled runs under load and disables a public repository's schedules after 60 days without activity
- [ ] each x86_64 record names the LAPIC timer mode the boot chose (`tsc-deadline`, `periodic`, or `pit`), and a KVM job fails when CPUID reports TSC-deadline and the boot chose another mode (F078)
- [ ] a corpus job runs the §20.2 host tests over the linuxhw/ACPI snapshot, QEMU's `tests/data/acpi`, and aslts, and the §20.7 device-tree tests, with every input fetched by hash and cached between runs
- [ ] a hung guest ends at the harness timeout with its §10.7 core uploaded, which is the power control a hosted runner needs
- [ ] the Phase 17 on-device `make test`, nightly, on vibeOS as a 4-vCPU, 4 GiB guest under KVM on the x86_64 runner; on aarch64 it would be TCG inside TCG on the arm64 runner, so its run is a §10.9 dev-host record under HVF

### 20.9 Firmware runtime and hotplug
§22.2's unattended updates choose their trial boot through UEFI variables, and a disk added to or pulled
from a running machine needs hotplug.

- [ ] Limine's EFI system table and EFI memory map responses captured in `BootInfo`; the runtime regions mapped in a page table of their own, with the choice between `SetVirtualAddressMap` and physical-mode calls recorded in DESIGN; calls made one at a time with interrupts off, on both architectures; around each call the live user FP and SIMD state is saved and FP and SIMD access is enabled (no `CR0.TS` or `CPACR_EL1` trap), since firmware may use those registers and the kernel is soft-float (§11.1, §11.6)
- [ ] `GetVariable`, `GetNextVariableName`, `SetVariable`, `GetTime`, and `ResetSystem`, with variables exposed as Linux's `efivarfs` at `/sys/firmware/efi/efivars` (`statfs` reporting `EFIVARFS_MAGIC`, which libefivar checks), so `efibootmgr` from the §14.9 mirror, added to its pin list, lists, reorders, and deletes boot entries and sets `BootNext` unmodified; creating an entry from a disk (`-c -d`) reads Linux's `/sys/class/block`, which is Phase 23
- [ ] native PCIe hotplug: slot capabilities, presence-detect and link-state interrupts, the attention button, slot power, and resource assignment for a new device from bridge windows sized with headroom at boot; the kernel asks for native control through `_OSC` and uses ACPI hotplug when firmware keeps it
- [ ] ACPI hotplug through bus-check and eject notifications and `_EJ0` (§20.2), which `q35` uses by default
- [ ] removal: in-flight I/O completes with an error, the driver's `remove` runs, its DMA mappings and interrupt vectors are released, and nothing panics; a surprise removal takes the same path, host-tested against a simulated slot whose presence detect drops without an attention-button press; removal follows DESIGN §2.11 rule 3's kill order, so no open descriptor or mount on the device delays it; removing a bridge or root port removes the devices below it first, deepest first (DESIGN §12.2)
- [ ] `irq_init::free_vector` returns only after no CPU is running the vector's top half and its threaded bottom half has finished or been cancelled and its §12.5 thread has exited (DESIGN §5.4), and `remove` stops the device, frees its vectors, and only then frees the state their handlers touch; an in-guest test frees a vector while another CPU's top half for it is held at a `kernel_tests` hook, and the free returns only after the hook releases it
- [ ] `remove` keeps §10.12's order: it writes device status 0, polls until it reads 0, and clears `COMMAND.MASTER` before it frees queue and data memory; an in-guest test removes a virtio-blk with `device_del` and reads status 0 and bus mastering off before its frames return to the buddy allocator (F116)
- [ ] persistent block device names from the serial number and NVMe namespace under `/dev/disk/by-id`, never from probe order
- [ ] the §18.1 `q35` harness configuration gains PCIe root ports, with a native-hotplug variant (`-global ICH9-LPC.acpi-pci-hotplug-with-bridge-support=off`), and the §11.7 aarch64 `virt` command line gains `pcie-root-port` devices, which hotplug natively. On both architectures a UEFI run loads the firmware as read-only code in pflash unit 0 and a writable per-run copy of its variable-store template in unit 1, the §10.2 probe locating the template beside each firmware image, so a variable survives a reboot into a new QEMU process on the same copy

### 20.10 Stretch: legacy USB hosts
- [ ] EHCI, with UHCI and OHCI companion controllers for low- and full-speed devices, tested under QEMU (`-device usb-ehci`, `ich9-usb-uhci1`, `pci-ohci`)

---

## Phase 21: Virtualization

**Goal.** Run other operating systems on vibeOS, and run vibeOS on vibeOS.

**Unlocks.** A hypervisor that Phase 22's CI boots test kernels in, and containers to isolate its jobs.
A conformance test for every paravirtual interface the kernel consumes as a guest. Unmodified OCI
images, run by `crun` or the native runtime.

The hypervisor gate needs hardware virtualization inside the guest vibeOS runs in. Three free
environments give it, and the gate lines name them.

**The nested job** runs with the nightly job on the hosted x86_64 runner, as a matrix of three legs,
each on its own runner. Each leg loads `kvm_intel` or `kvm_amd` with `nested=1` and boots vibeOS under
KVM with `-cpu host`, which passes the runner's VMX or SVM through. GitHub calls nested virtualization
on its runners experimental and does not support it, and it draws the CPU vendor per job (AMD with SVM
or Intel with VMX), so a leg tests the path its CPU offers, and when §20.8's host-side probe finds
neither on its runner it skips, naming the CPU model in the job summary. A gate line in the nested job passes when a leg passed,
not skipped, at the gated commit, and the nightly job's last 7 runs include a passing leg of each path
that §20.8's records of those nights show a runner offering (§21.1).

**The EL2 job** runs with the nightly job on the hosted arm64 runner, which has no KVM. It boots
vibeOS under TCG with `-machine virt,gic-version=3,virtualization=on -cpu max` on the §10.1 pinned
QEMU, so vibeOS enters at EL2 (§11.1). From QEMU 9.0, TCG's `max` CPU also has FEAT_NV2, which §21.3's
aarch64 nesting uses.

**The HVF record** is a §10.9 dev-host record of the aarch64 lines under HVF, with
`-machine virt,virtualization=on -accel hvf -cpu host` on QEMU 11.1 or later, the first release whose
HVF gives a guest EL2 (on M3 or later with macOS 15 or later; the dev host is an M4 Pro). It covers one
level of guests; nesting under HVF is not claimed. §10.1's QEMU version check does not reach the dev
host, so its gate-map entries fail on a record made with a QEMU older than 11.1.

In each, the vibeOS host (L1) is a 4-vCPU, 4 GiB guest, the Phase 17 shape, and its guests (L2) have 2
vCPUs and 1 GiB unless a line names another shape. A comparison with Linux KVM boots §21.3's Linux L1
image in the same shape, with the same QEMU and the same L2 guest, in the same job run or record
session, so the runner's noise cancels. Throughput and isolation thresholds are ratios measured within
one nested-job leg or one HVF record session, and hold on whichever CPU model a leg draws; the EL2 job
records its numbers without a threshold, since TCG's costs are not a CPU's. TCG's `-cpu max` on x86_64
has SVM with nested paging, so early SVM work runs on any host, but TCG has no VMX and incomplete SVM
event injection, and no gate line rests on it. If §20.8's records show no hosted runner offering a guest
VMX or SVM for 7 nights running, GitHub has withdrawn nesting, and the x86_64 hypervisor lines here,
Phase 22's x86_64 hostile-guest campaign, and the x86_64 parts of Phase 28's VF-assignment line and
Phase 30's live-update line move to [Funded goals](#funded-goals) by an edit to this file; those two
lines then close on their aarch64 parts.

**Architectures.** Both: VMX and SVM on x86_64, EL2 on aarch64, behind one VM abstraction. The x86_64
hypervisor lines run in the nested job, and the aarch64 ones in the EL2 job and the HVF record, with
aarch64 nesting (§21.3) in the EL2 job only. The container lines (§21.5, §21.6) need no hardware
virtualization and run in the hosted tiers under TCG on both. §21.4's CPUID detection, paravirtual clock, and
paravirtual spinlocks are x86_64 only: arm64 KVM offers no paravirtual clock or spinlock interface,
because the generic timer is already virtualized, so on aarch64 the guest uses the virtual generic timer
and SMCCC stolen time. §21.4's VMware line is x86_64 only too, since VMware's guest interface is CPUID
leaves; its Hyper-V line covers both. §21.8's emulator differential test covers x86_64's instruction
emulator and, on aarch64, the loads and stores that exit without a valid syndrome.

**Exit gate**
- [ ] a Linux kernel boots to userspace as an L2 guest under vibeOS, in the nested job, the EL2 job, and the HVF record
- [ ] vibeOS boots to `shell ready` as an L2 guest under vibeOS, in the nested job, the EL2 job, and the HVF record; in the nested job and the EL2 job, that guest boots a third vibeOS (2 vCPUs, 256 MiB) to `shell ready` under its own §21.2 VMM
- [ ] L2 guests get virtio block and network devices whose sequential throughput is at least half of what the same L2 guest gets under §21.3's Linux L1, in the nested job and the HVF record; the EL2 job records both numbers
- [ ] an unmodified static `crun` release runs an OCI bundle with pid, mount, uts, ipc, network, and user namespaces and a cgroup v2 `memory.max` that the OOM path enforces, on both architectures
- [ ] vibeOS as a guest of Linux KVM uses the §21.4 paravirtual interfaces for its architecture. On the §10.1 KVM leg, a 4-vCPU, 2 GiB vibeOS guest with its vCPU threads pinned to 2 host CPUs runs the §19.3 lock-acquire microbenchmark on every vCPU at 1.5 times or more the throughput it reaches with paravirtual spinlocks off, best of three runs each, in one job. In the EL2 job, vibeOS as an L2 guest of the Linux L1 reads stolen time through SMCCC `PV_TIME` that grows while a CPU-bound task in L1 shares its vCPU's CPU
- [ ] the harness boots a test kernel under the §21.2 VMM with `-accel kvm` through the §21.2 harness backend, and `make test-kernel` passes that way on vibeOS, in the nested job, the EL2 job, and the HVF record
- [ ] `alpine` and `busybox` OCI images, pinned by digest and pulled from the §21.6 registry on the CI host, run a shell under the §21.6 runtime, and the Alpine one installs a package with `apk` from a local mirror, on both architectures
- [ ] a container and an L2 guest on one §21.7 bridge each fetch a file from a server on the CI host through NAT, and a filter rule blocks a named port for the container only, in the nested job and the EL2 job
- [ ] a hostile L2 guest with 2 vCPUs and 1 GiB fuzzes the exit handlers and virtio backends for 24 hours a week per architecture on the weekly job, in shards of at most 5.5 hours (§10.1) that carry one seed log and coverage corpus as artifacts, set up as the nested job on x86_64, with each path §20.8's records show offered among the last two weeks' shards, and as the EL2 job on aarch64, with no L1 panic and no KASAN report; the instruction emulator agrees with the CPU on 1,000,000 generated instruction streams on each hosted runner architecture; and 10,000 of the fuzzing guest's seeded exit sequences end in the same guest-visible state under the Linux L1's KVM as under the §21.2 VMM, each pair replayed in one nested-job leg or one EL2-job run, except the differences `docs/` lists (§21.8)
- [ ] tag `phase-21` and cut the next release

### 21.1 Hypervisor
- [ ] VMX and SVM detection and enablement, with the feature MSR checks
- [ ] a VMCS or VMCB per vCPU, with guest and host state areas
- [ ] the VM entry and exit path, and exit reason decoding
- [ ] x86_64: the §18.3 speculation posture at VM entry and exit, each part chosen from the §18.3 enumeration and reported as Linux reports `l1tf` and `mds` for a host: an IBPB when a CPU switches between two guests' vCPUs, an RSB fill after VM exit, `IA32_FLUSH_CMD.L1D_FLUSH` before VM entry on a CPU that enumerates it and lacks `SKIP_L1DFL_VMENTRY`, and `verw` before VM entry where MDS applies, and after a VM exit, an IBPB before that CPU's next return to user mode, on a CPU the enumeration marks affected by VMScape (`vmscape`), so a guest cannot train the branch predictor §21.2's QEMU then runs with; each is measured in the nested job (F131, F132)
- [ ] EPT or nested paging for guest physical to host physical translation
- [ ] guest interrupt injection, virtual APIC, and posted interrupts where available
- [ ] MSR and I/O bitmaps, and instruction emulation for the exits that need it
- [ ] a vCPU as a schedulable entity, so the existing scheduler runs guests
- [ ] aarch64: run as a VHE host when §11.1 recorded EL2 entry, with stage-2 translation and the virtual GIC and timer behind the same VM abstraction; when entry was at EL1, the VM layer reports that EL2 is unavailable instead of failing
- [ ] every hypervisor test runs on whichever of VMX and SVM the CPU offers and prints a registered marker naming the path; the nested job writes the runner's CPU model and that path to its §10.9 CI-history record, and `scripts/ci_history.py --nested`, which the gate-map entries of the nested-job lines run, fails unless the nightly job's last 7 runs include a passing leg of each path that §20.8's records of those nights show a runner offering, and a failing leg on an offered path still fails; when one is missing, it reports from the nesting field of the same nights' §20.8 x86_64 KVM records whether any runner drawn offered a guest that path, so a vendor the fleet did not offer is told apart from a leg that failed

### 21.2 Virtual machines
The VM interface is Linux's: `/dev/kvm`. The VMM is unmodified QEMU from the §17.6 image, so guests
get the devices, firmware, and management interface the ladder already uses, and "the §21.2 VMM" below
means QEMU with `-accel kvm` on vibeOS.

- [ ] `/dev/kvm` with Linux's `KVM_*` ioctl ABI over §21.1: VM and vCPU file descriptors, memory slots, `KVM_RUN` with the shared `kvm_run` page, `irqfd` and `ioeventfd`, the capability queries QEMU makes, and each architecture's register ABI, so QEMU runs guests with `-accel kvm`; `/dev/kvm` is mode 0660, group `kvm`, so only a user root adds to that group runs a VM, and an open by uid 1000 outside that group fails with `EACCES`
- [ ] guest memory as an address space, with host page faults servicing guest access
- [ ] the in-kernel interrupt controller and timer as QEMU creates them. On x86_64, the `pc` default uses `KVM_CREATE_IRQCHIP` (local APIC, I/O APIC, and PIC) and `KVM_CREATE_PIT2`, and §18.1's `kernel-irqchip=split` `q35` configuration uses `KVM_ENABLE_CAP(KVM_CAP_SPLIT_IRQCHIP)`, which keeps only the local APIC in the kernel and leaves the I/O APIC, PIC, and PIT to QEMU. On aarch64, `KVM_CAP_DEVICE_CTRL` and `KVM_CREATE_DEVICE`, including the test-create QEMU probes the GIC version with, for `KVM_DEV_TYPE_ARM_VGIC_V3` and `KVM_DEV_TYPE_ARM_VGIC_ITS` with their `KVM_SET_DEVICE_ATTR` groups, `KVM_SIGNAL_MSI` with device IDs for the in-kernel ITS, and the per-vCPU generic timer
- [ ] `vhost-net` and `vhost-vsock` in the kernel, so QEMU's virtio devices reach the gate's throughput without a userspace hop
- [ ] the harness backend: on vibeOS, `VIBEOS_QEMU_ACCEL=kvm` selects `/dev/kvm`, so `make test-kernel` boots its test kernels with the devices, firmware (SeaBIOS, OVMF, edk2), and `isa-debug-exit` or pvpanic verdict path it uses everywhere else; where VMX, SVM, or EL2 is absent it falls back to TCG
- [ ] guests managed through QEMU's QMP socket: create, start, stop, pause, inspect

### 21.3 Nesting
- [ ] vibeOS on vibeOS, which mostly tests that the paravirtual interfaces are honest
- [ ] Linux as a guest, which is the real conformance test of the hypervisor
- [ ] nested virtualization, so a guest can itself be a hypervisor: VMX on VMX and SVM on SVM, each tested in the nested job on runners whose CPU has it, and on aarch64 a guest hypervisor at virtual EL2 through FEAT_NV2, tested in the EL2 job
- [ ] a documented performance comparison against Linux KVM, from the nested job and the HVF record, with the gaps explained rather than hidden
- [ ] the Linux side of every comparison, pinned: for the Linux-guest lines, Alpine's `linux-virt` for each architecture from the §14.9 mirror, added to its pin list, with an initramfs built on the host from the §14.9 minirootfs; for the lines that compare with Linux KVM, the Linux L1 image, Alpine's `linux-lts` with the QEMU, firmware, and CPython the §17.7 image pins, the harness, and the §21.8 hostlib runner, which the nested job, the EL2 job, and the HVF record boot in the vibeOS L1's shape, so each comparison runs the same VMM and L2 guest on the same runner

### 21.4 Guest support
- [ ] x86_64: detect running under a hypervisor through CPUID leaf `0x40000000`
- [ ] x86_64: KVM paravirtual clock, so timekeeping is not calibrated against a lying TSC
- [ ] x86_64: paravirtual spinlocks through `KVM_FEATURE_PV_UNHALT` and the `KVM_HC_KICK_CPU` hypercall, so a waiter whose lock holder's vCPU is preempted halts instead of spinning. A `SpinMutex` waiter that has spun a bound marks the lock contended, records it in its CPU's wait slot, checks the lock and its incoming work once more, and halts with IF=0; a release that finds the lock contended kicks one CPU whose slot names it. A halted waiter still services IPIs (DESIGN §2.3, §2.9 rule 2): every publisher of work that `service_incoming` serves (a shootdown or call-function slot, and any request word §10.7 adds for it to read) kicks a target whose slot is set, after its Release store and a full fence. If §27.5's queued lock has landed first, the kick hooks its queue instead of wait slots. Linux's `nopvspin` on the §10.2 command line turns the halt off, which the Phase 21 gate's comparison uses. An in-guest test on the §10.1 KVM leg, with every vCPU thread pinned to one host CPU, has CPU 1 wait on a `SpinMutex` that CPU 0 holds while CPU 0 unmaps a KVA range, and the unmap's shootdown completes with no `wait_acks` late-CPU line
- [ ] a guest whose vCPUs the host deschedules does not panic; §10.10's `wait_acks` logs a late CPU and keeps waiting. On the §10.1 KVM leg, a 4-vCPU guest with every vCPU thread pinned to one host CPU runs the §4.10 shootdown test and brings every AP online, with no panic, no `wait_acks` late-CPU line, and no `smp: apic <id> timed out` (F032)
- [ ] aarch64: hypervisor detection through the SMCCC vendor-hypervisor UID call, and stolen time through SMCCC `PV_TIME`; the virtualized generic timer needs no paravirtual clock
- [ ] balloon driver for memory reclaim by the host
- [ ] Hyper-V detection on both architectures: the `Microsoft Hv` signature at CPUID `0x40000000` and its feature leaf `0x40000003` on x86_64; on aarch64, the FADT hypervisor vendor identity `MsHyperV` (§20.7's ACPI) or Hyper-V's UID from the SMCCC vendor-hypervisor call above; the detected hypervisor is logged. An in-guest test on the §10.1 KVM leg with `hv-time` and `hv-frequencies` on the `-cpu` line finds Hyper-V, and a host test covers the aarch64 path against a recorded FADT. Hyper-V's hypercalls, SynIC, and reference TSC page are §26.5's
- [ ] x86_64: VMware detected by its `VMwareVMware` signature at CPUID `0x40000000`, with the TSC frequency from leaf `0x40000010`; host-tested against recorded CPUID values, since QEMU does not present VMware's signature

### 21.5 Containers
- [ ] pid, network, uts, ipc, user, and cgroup namespaces beside §18.6's mount namespace, through Linux's interfaces: `CLONE_NEW*` flags on `clone` and `unshare`, `setns`, and `/proc/<pid>/ns/*`, `mnt` included; user namespaces with `uid_map`, `gid_map`, and `setgroups`, and §18.6's file capabilities in revision 3 (`vfs_ns_cap_data`), whose root uid a namespace maps; mount propagation (`MS_PRIVATE`, `MS_SLAVE`, `MS_SHARED`) as Linux defines it; creating a user namespace needs no privilege on Linux and is bounded per user by `user.max_user_namespaces` (`/proc/sys/user/max_user_namespaces`, writable by root), which at 0 makes `clone` and `unshare` with `CLONE_NEWUSER` fail with `ENOSPC`, and vibeOS's default for it is the owner's answer to the block below. `docs/THREAT_MODEL.md` lists, as a known escalation path, every check a namespace's root passes that reaches kernel code otherwise open only to root: each filesystem that allows a mount inside a user namespace, the §21.7 `nf_tables` netlink, and every other capability check made against a namespace. In-guest as uid 1000: with the limit at 1, a process creates a user namespace and maps its own uid; with it at 0, it gets `ENOSPC`

> **OWNER DECISION NEEDED (review J076)**: may a user without privilege create a user namespace? Inside
> one, a process holds every capability over what the namespace owns, and that reaches kernel code
> otherwise open only to root: filesystem mounts, the §21.7 `nf_tables` netlink, and every other check
> made against a namespace's capabilities. On Linux that reach has been the largest single source of
> local privilege escalations. Options: (a) allowed, as on Linux: `user.max_user_namespaces` at Linux's
> default, so the §36.4 browser's sandbox, bubblewrap under §36.7's Flatpak, and rootless containers
> work unmodified; `docs/THREAT_MODEL.md` lists the reach above as a known escalation path accepted by
> this decision, narrowed by the uid 1000 tests and the syzkaller `sandbox: namespace` campaign below.
> (b) Refused by default: `user.max_user_namespaces` is 0 until root raises it, stricter than Linux;
> the browser's sandbox runs weaker or needs a set-user-ID helper, rootless containers need root to
> turn it on, and nothing else changes. Recommendation: (a), because vibeOS runs unmodified Linux
> software that expects it, and (b) moves the risk into weaker browser sandboxing rather than removing
> it. The box above builds and tests both settings; until the owner answers, the default is 0, the
> stricter one.

- [ ] from here on, the §18.5 weekly syzkaller campaign runs half its hours with `sandbox: namespace`, with `user.max_user_namespaces` raised for the run, so the fuzzer reaches the checks a namespace's root passes; the job summary records corpus coverage for each sandbox
- [ ] cgroup v2 as Linux's `cgroup2` filesystem at `/sys/fs/cgroup`, whose `statfs` reports `CGROUP2_SUPER_MAGIC`, which crun checks: `cgroup.controllers`, `cgroup.subtree_control`, `cgroup.procs`, `cgroup.type`, `cgroup.events`, `cpu.max`, `cpu.weight`, `memory.max`, `memory.current`, `memory.events`, `io.max`, and `pids.max`; backed by §19.4's group scheduling, by pages charged to the allocating task's cgroup and uncharged on free, with reclaim and then the §12.6 OOM killer, with the group as its scope, at its `memory.max`, and by `io.max` bandwidth and IOPS limits applied in the §7.1 request queue
- [ ] `/proc/<pid>/cgroup`, with the path relative to the reader's cgroup namespace, and `/proc/<pid>/mountinfo` with its `shared:` and `master:` propagation fields, in Linux's formats; the files themselves are built by whichever of §21.5 and §23.4 lands first, and the namespace-relative path and the propagation fields here
- [ ] overlayfs as Linux's `mount -t overlay`: `lowerdir`, `upperdir`, and `workdir`, whiteouts as character device 0/0, and opaque directories, so image layers stack as they do on Linux
- [ ] overlayfs's opaque directories and whiteouts on §14.8's `trusted.` extended attributes, on vibefs v2 and tmpfs

### 21.6 OCI images and the Linux container ABI
§21.5 builds namespaces, cgroup v2, and overlayfs to Linux's interfaces. This makes unmodified OCI images
and tools run on them.

- [ ] an OCI image and distribution client: pull by digest over §15.11 HTTPS, verify every layer's digest, and unpack each layer with its whiteouts onto §21.5's overlayfs
- [ ] an OCI registry on the CI host serving the pinned test images over TLS with the §15.11 test CA, which the guest trusts only through §14.3's harness overlay, so no CI job pulls from the public internet
- [ ] a container runtime with the OCI runtime command line (`create`, `start`, `state`, `kill`, `delete`) that runs runtime-spec bundles (`config.json`): namespaces, mounts and `pivot_root`, rlimits, the §18.6 capabilities and `seccomp` profile, cgroup placement, and process launch
- [ ] the opencontainers `runtime-tools` validation suite, pinned, passes in-guest against the runtime on both architectures, with a checked-in skip list whose entries each name a reason
- [ ] a runtime tool with `pull`, `run`, `exec`, `ps`, `stop`, and `rm` over the client and the runtime

### 21.7 Virtual networking
- [ ] Linux's TUN/TAP device: `/dev/net/tun` with `TUNSETIFF`, `IFF_TAP`, and `IFF_NO_PI`, which the §21.2 virtio-net backend and unmodified userspace attach to the same way
- [ ] with vibeOS as the harness host, §15.10's tap-device tests run and pass, and their entries leave the DESIGN §8.6 on-device skip list
- [ ] veth pairs and a software bridge with MAC learning, shared by §21.5's network namespaces and by guests, managed unmodified by iproute2 from the §14.9 mirror: added here to §15.7's `NETLINK_ROUTE` are `RTM_NEWLINK` with `IFLA_LINKINFO` kinds `veth` and `bridge`, ports attached with `IFLA_MASTER`, a veth end moved into a network namespace with `IFLA_NET_NS_PID` or `IFLA_NET_NS_FD`, and `RTM_DELLINK`
- [ ] a stateful packet filter in the §15.4 IP layer: rules on interface, address, port, protocol, and connection state, over a connection tracker with timeouts and a bound on tracked connections
- [ ] NAT on the connection tracker: masquerade for outbound traffic and port forwarding for inbound, so a container or guest reaches the outside through the host's address
- [ ] the filter and NAT rules are Linux's nf_tables over `NETLINK_NETFILTER`, so unmodified `nft` from the §14.9 mirror loads a rule file at boot and lists rules with their hit counters

### 21.8 Hostile guests
Every other test in this phase uses a cooperative guest. A guest controls every exit reason, every MMIO
and port access, every virtio descriptor, and every instruction the emulator decodes.

- [ ] a fuzzing guest: a small vibeOS kernel that generates exits (port and MMIO access, `cpuid`, MSR and system-register access, hypercalls, faults during event delivery) and malformed virtio descriptor chains, seeded and logged so a host failure replays from its seed
- [ ] the host runs the §12.1 KASAN build with §18.5's KCOV over the VMM, and the coverage steers the fuzzing guest's generator between runs
- [ ] the x86_64 instruction emulator differential-tested against the CPU: generated instruction streams run natively and through the emulator from the same registers and memory, and the final states compared. The emulator is in the portable half, so the native side is a host process on the hosted x86_64 runner, whose CPU is real, as well as a vibeOS user process on the §10.1 KVM leg; each divergence minimized to one instruction and checked in
- [ ] aarch64 decodes MMIO from the `ESR_EL2` syndrome and emulates only the loads and stores without a valid syndrome, which get the same differential test, natively in a host process on the hosted arm64 runner
- [ ] the fuzzing guest's seeded exit sequences, other than the virtio ones, replayed under Linux KVM through a hostlib runner on the Linux L1's `/dev/kvm` (§21.3), in the same nested-job leg or EL2-job run as their replay under the §21.2 VMM; in that mode both VMMs answer every MMIO and port access from the seed instead of a device model, and the guest-visible state after each exit (registers, memory, and any injected exception or interrupt) is compared; each difference is fixed or listed in `docs/` with KVM's behavior and the reason
- [ ] a guest that exhausts its memory, vCPU time, or virtio queues affects only itself: while the first misbehaves, a second guest's §19.3 microbenchmarks stay within 10% of its numbers beside an idle first guest, each guest 2 vCPUs and 1 GiB, in the nested job and the HVF record; the EL2 job records the numbers

---

## Phase 22: Distribution

**Goal.** Something another person can install and run, released by a process that runs on itself.

**Unlocks.** Users. An installable, upgradable system with a release process, which the later eras
ship through and [Phase 39](#phase-39-stability) turns into 1.0.

**Architectures.** Both. Every artifact is built, signed, and installable for each. Every gate line runs
under QEMU with UEFI firmware, x86_64 on `q35` with OVMF under KVM on the hosted x86_64 runner and
aarch64 on `virt` with edk2 under TCG on the hosted arm64 runner, unless it names another setup. The
§22.4 agent runs in a vibeOS guest on the hosted x86_64 runner, where it builds and tests both
architectures (§24.1 adds its arm64 runner's job); the native aarch64 build runs under HVF on the dev
host as a §10.9 record, as in Phase 17. Live images, installs, and CI on physical machines, and the physical section of
the §22.3 tested-platforms list, are [Funded goals](#funded-goals).

**Exit gate**
- [ ] a live image for each architecture boots to a graphical desktop that matches its §16.1 reference image, attached as a `usb-storage` disk on `qemu-xhci`, as a user writes it to a USB stick, with a `usb-kbd` and a `usb-tablet` as its only input devices; the x86_64 image also boots as an AHCI CD-ROM
- [ ] the installer, run from the live image, partitions a blank NVMe disk, installs, and produces a system that boots to its desktop with the live image detached: once unattended from a configuration file, and once interactively, driven through QMP `input-send-event` and checked against a §16.1 reference image at each screen
- [ ] a release is built reproducibly: before signing, the artifacts the §22.4 build on vibeOS produces are byte-identical to those a hosted Linux job builds from the same commit with a different checkout path, `CARGO_HOME`, rustup home, user id, and clock (F151, F152)
- [ ] artifacts are signed and verified on install
- [ ] the CI that gates releases runs on vibeOS: in the nightly job, the §22.4 agent on an installed vibeOS, a 4-vCPU, 4 GiB guest under KVM on the hosted x86_64 runner, builds both architectures and runs `make test` for both, and the job's conclusion is the result; `release.yml`'s `build` job reads that conclusion through the Actions API for the nightly workflow's path, a `schedule` or `workflow_dispatch` event, and the candidate's commit, never from a commit status, which any job with `statuses: write` can post, and refuses a candidate without a passing one
- [ ] a fresh install builds and releases vibeOS: in the release workflow's `build` job, a fresh unattended install from the candidate's installer image, a 4-vCPU, 4 GiB guest under KVM on the hosted x86_64 runner, builds the release's artifacts for both architectures; the `sign` job signs those only after the reproducibility line above holds for them, a keyless `verify` job checks the signed artifacts on vibeOS, and those are the ones published (§22.4); a fresh aarch64 install does the same build under HVF on the dev host as a §10.9 record, unsigned, since the keys stay with the workflow
- [ ] every release artifact ships an SPDX SBOM that passes the validator, and the release job refuses a component without an entry or with a license outside the §14.10 policy
- [ ] no release is tagged with an open crash from its candidate's two 72-hour campaigns, both in hosted shards of at most 5.5 hours (§10.1) that carry one seed log and coverage corpus as artifacts: syzkaller (§18.5) accumulating 72 hours per architecture, each guest 2 CPUs and 512 MiB under TCG, and the §21.8 hostile guest, 2 vCPUs and 1 GiB, accumulating 72 hours per architecture, set up as Phase 21's nested job on x86_64, with each path §20.8's records show offered among its shards, and as its EL2 job on aarch64
- [ ] an update installs unattended and takes effect on the next boot; three bad updates, one that panics, one that hangs with interrupts off, and one that fails its health check, each come back on the old root with no human action, on both architectures under QEMU with OVMF or edk2 and a writable variable store (TCG, 2 vCPUs, 2 GiB)
- [ ] the GitHub API reports private vulnerability reporting enabled on the repository, the §22.5 `make check` script passes, and the §22.5 drill has run end to end
- [ ] tag `phase-22` and cut the next release

### 22.1 Release engineering
- [ ] versioning with a defined policy for what constitutes a break
- [ ] reproducible builds extended from the §10.2 kernel, initrd, and ISO build to every release artifact (packages, installer images, manifests): no timestamps, no paths, no nondeterministic ordering
- [ ] the §14.6 signed release manifest extended to the installer images and the package repository of both architectures, with §18.7's boot-chain digests in it, and §14.6's version and expiry in it
- [ ] a release branch and backport process; the release build interface, `make release-artifacts OUT=<dir>`, stays the same on every supported branch, since `main`'s `release.yml` runs it at each branch's tag (§10.1)
- [ ] release notes generated from the changelog, which is what the changelog discipline was for
- [ ] an SPDX SBOM for each release artifact, generated from `Cargo.lock`, the §14.6 package metadata, and the §14.10 provenance records: every component with its version, license, and source hash, checked by a pinned SPDX validator in the release job
- [ ] the release job refuses a shipped component with no SBOM entry or with a license outside the §14.10 policy, and a shipped package without its license texts under `/usr/share/licenses/<package>`
- [ ] each release candidate gets both 72-hour fuzz campaigns the exit gate names, seeded with the weekly corpora; a crash found against a candidate blocks its tag until it is fixed with a regression test
- [ ] from `v0.14.0` on, the release workflow records each signing run, a release or a §14.6 metadata-only re-sign, with its date, in the §10.9 `ci-history` records; when the records show installed metadata expiring before a re-sign ran, or a year that needed more metadata-only re-sign runs than releases, the §14.6 expiry window and re-sign schedule are revisited in an edit to this file that cites the records

### 22.2 Installation
- [ ] a live ISO with a full desktop and an installer
- [ ] the harness boots images as a user would: the live image as a `usb-storage` disk on `qemu-xhci`, and on `q35` also as an AHCI `ide-cd`, and the installed system from NVMe, with a `usb-kbd` and a `usb-tablet` through §20.3's HID drivers as the input devices; it drives the interactive installer through QMP `input-send-event` and checks each screen against a §16.1 reference image
- [ ] partitioning: automatic and manual, GPT with a UEFI system partition
- [ ] Linux's `BLKRRPART`, so the installer rereads the table it wrote and the new partitions appear as block devices without a reboot; built by whichever of §22.2 and §23.3 lands first; it returns `EBUSY` while any partition of the disk is open or mounted, as Linux's does, and otherwise removes the old partitions as DESIGN §12 removes a device, so the new ones get new ids
- [ ] filesystem creation, base system install, bootloader install
- [ ] user creation, locale, timezone, network configuration
- [ ] an unattended install from a configuration file, which is how CI installs it
- [ ] upgrades between releases through the A/B update below, tested from every supported prior version
- [ ] recovery: a rescue shell and `fsck` on boot, and the other slot's boot entry kept in `BootOrder` after a commit, so the previous release stays bootable from the firmware menu
- [ ] two slots, A and B, created by the installer: each a root partition plus an ESP directory holding its own kernel, initrd, Limine binary, and Limine configuration, since Limine reads only FAT and ISO 9660 and cannot load a kernel from the root; each slot has its own UEFI boot entry for its Limine binary, which reads the configuration beside it (Limine 10.3 and later); a slot's configuration names its root with Linux's `root=PARTLABEL=vibeos-root-a` (or `vibeos-root-b`) and carries `panic=` at one value per release, so it holds no per-install value and a release ships one signed Limine binary per slot and architecture, its configuration enrolled (§18.7); the kernel matches the label only on the disk whose GPT disk GUID Limine's executable file response names, and only on a partition of the vibeOS root type, and refuses the boot with a named reason when two partitions match; the installer writes each entry itself as a `Boot####` load option through §20.9's efivarfs, an `HD()` node (the ESP's partition number, start LBA, size, and unique partition GUID) and a `File()` node for that Limine binary, and adds it to `BootOrder`, since `efibootmgr -c -d` reads Linux's `/sys/class/block`, which is Phase 23. An update writes the inactive slot from the §14.6 packages its signed release manifest names, fetched over §15.8 HTTP and verified against that manifest before anything is written. The updater refuses a manifest for a release older than the installed one, under §22.1's versioning policy, unless root passes an explicit downgrade flag, and refuses an expired manifest for an automatic update (§14.6); the trial-boot fallback to the old slot is local and not affected. An in-guest test offers the updater the previous release's manifest, and it refuses
- [ ] with Secure Boot on under §18.7's throwaway PK, KEK, and db, a fresh install boots each slot through its own signed Limine binary on both architectures, and a second disk carrying a partition labelled `vibeos-root-a` of the vibeOS root type does not become root; a Limine that cannot boot a slot from an enrolled configuration naming its root by partition label overturns DESIGN §3.2's root-naming rule
- [ ] a trial boot: the updated slot boots once through UEFI `BootNext` (§20.9), and is committed by rewriting `BootOrder` only when a declarative health check passes (services up under §14.3's init, the network reachable, and a probe each service defines); a failed check reboots
- [ ] `panic=<seconds>` on the §10.2 command line, set in each slot's Limine configuration at one value per release, so the enrolled configuration holds no per-install value: the panic handler prints its dump, waits, and resets through the ACPI or PSCI path the §10.5 `reboot` call uses; halting stays the default, so the harness and the Phase 0 panic gate are unchanged. The reset register is the one §20.2 maps at boot, so the panic path maps nothing and takes no lock; on x86_64, if the machine still runs 1 s later, it pulses the 8042 reset line and then resets through port `0xCF9` (F097)
- [ ] §20.6's `i6300esb` driver armed at boot and fed by init for as long as the system runs, so a hang during a trial boot, or after one, resets the machine
- [ ] a trial boot that panics, hangs, or fails its health check comes back on the old root at the next reset with no human action, since `BootNext` lasts one boot; Phase 25 extends the panic policy and the watchdogs
- [ ] `docs/NETWORK.md` lists every connection an image a release ships makes without a user action: its destination, what it sends, why, its default, and the switch that turns it off; each defaults as the record below sets it, and until that record exists every one is off; the installer's summary names each connection that is on, with its switch. The first entries are the §22.2 update check and §15.8's time sync, and later phases add theirs (§37.2's desktop services, a cloud image's metadata service). `scripts/check_network_doc.py` in `make check` fails on an entry without those parts, with host tests. In the nightly job, each image a release ships boots on a QEMU user network with `restrict=on`, which reaches nothing, while a `filter-dump` capture records every DNS query and connection attempt for 10 minutes after boot, idle and through one login; a host script lists the destinations, the job fails on one `docs/NETWORK.md` does not name, and the release workflow refuses a candidate without a passing run

> **OWNER DECISION NEEDED (review J028)**: which connections a shipped image makes on its own. Each
> tells the host it contacts at least the machine's IP address. `docs/NETWORK.md` lists them, and until
> your answer is recorded here every one is off in every image a release ships; the harness turns on
> what its tests need. Recommended: (1) On, each named by the installer and in `docs/NETWORK.md` with
> one switch that turns it off: the §22.2 update check against this repository's GitHub Releases, since
> a system that never checks misses security fixes; and time sync from the NTP servers DHCP offers, or
> else one default server `docs/NETWORK.md` names, since TLS and signed update metadata need a correct
> clock. (2) Off: NetworkManager's connectivity check (no project server exists at $0, and a third
> party's would learn of every network change); GeoClue's location service; the browser's telemetry,
> studies, and own updater (the browser updates as a §14.6 package); the software center's ODRS reviews;
> a Flathub remote; debuginfod. (3) Your call, with no recommendation: the browser's safe browsing,
> which sends hash prefixes of visited addresses to the browser vendor's service and in return warns
> about known phishing and malware sites. The other options: everything off, which leaves each user to
> turn on updates and time sync; or upstream defaults, disclosed in `docs/NETWORK.md`. Nothing before
> Phase 22's first installable release depends on the answer.

### 22.3 Documentation
- [ ] an installation guide and a user handbook
- [ ] a developer guide covering the build, the test tiers, and the subsystem docs in this directory
- [ ] the tested-platforms list, generated into `docs/HARDWARE.md` beside §20.1's model list from the release's §10.9 gate records: an entry for each QEMU machine type, accelerator, firmware build, and device-model set the gate ran on, and the dev host's macOS and QEMU versions for its records; its entries are virtual machine configurations, and it claims no physical machine
- [ ] man pages for everything shipped
- [ ] a known-issues list that includes every open box in every shipped phase, generated from this file

### 22.4 The loop
- [ ] a CI agent on vibeOS: a workflow job boots an image the §22.2 unattended installer produced, under KVM on the hosted x86_64 runner (§24.1 adds the arm64 runner's job, under TCG), and passes in the commit and the commands to run, and no token or secret, since the guest runs candidate code; the agent checks the commit out over HTTPS with no credential, since the repository is public, runs the commands, such as the ladder with test kernels booted under the §21.2 VMM (TCG where the guest has no VMX, SVM, or EL2), and hands each command's exit status, log, and artifacts back to the job, which uploads them to the workflow run and concludes with their result
- [ ] CI running on vibeOS in every nightly job: checkout, build, test, and the artifacts published to the workflow run, with the agent's per-step times in its §10.9 CI-history record
- [ ] a release built and verified on vibeOS and signed on the host: the §22.4 agent builds the release in `release.yml`'s `build` job; the `sign` job signs the artifacts with §14.6's release key and §18.7's db key on the hosted runner, under §14.6's key-job rule, only when they match the hosted Linux build byte for byte; and a `verify` job with no key and `contents: read` boots vibeOS, checks every signature with the §14.7 verifier, and must pass before the `publish` job runs. No key enters a guest
- [ ] the resulting installer image installed unattended onto a blank disk in a new guest, which then builds the next release
- [ ] the whole thing scripted and documented so it is a procedure rather than a story

### 22.5 Security process
§18.8's `SECURITY.md` names a reporting channel and a triager. With users, reports follow the procedure below.

- [ ] private vulnerability reporting enabled on the repository, a setting only the maintainer can change, and `SECURITY.md` naming how to report, what is in scope, the supported releases, and the response times
- [ ] an embargo procedure in `SECURITY.md`: the fix developed in the advisory's temporary private fork and tested with `make test` before merge, released to `main` and every supported release branch at once, with the GitHub security advisory published and a CVE identifier requested through GitHub
- [ ] every advisory names the affected releases (from the §22.1 SBOM for a third-party component), the release that fixed it, and the regression test that proves the fix
- [ ] a drill: a planted bug reported through private reporting and taken through the embargo procedure to a draft advisory that is closed rather than published, with a record in `docs/` linking the report, the fix, and the test
- [ ] a script in `make check` verifies that `SECURITY.md` and the drill record have the parts this subsection names

---

# Era V. Ecosystem

Past the first destination: other people's software, unmodified and judged by its own test suites
against Linux running the same binaries, then a build that takes no upstream binary on trust.

Phase 23 needs 16 and 17 and nothing from Era IV, so it runs beside Phases 18 to 22. A suite case that
needs an Era IV feature (namespaces, seccomp, `io_uring`, `splice`, PI futexes) stays on its
expected-failure list, naming the line in Phases 18 to 22 that lands it, and is left out of its suite's
gate share until that phase is tagged. Phase 24 needs 22, whose §22.4 CI agent runs its builds in vibeOS
guests on hosted runners (§24.1 adds the agent's aarch64 job), and 23, whose Debian gcc its diverse
double-compiling check uses. Both phases run on GitHub-hosted runners alone, x86_64 guests under KVM and
aarch64 guests under TCG, and split a run longer than a hosted job's 6 hours into shards of at most 5.5
hours (§10.1).

## Phase 23: Linux Compatibility

**Goal.** Unmodified Linux software runs on vibeOS and is judged by suites vibeOS did not write, each
compared against Linux running the same binaries in the same guest. glibc joins musl and Debian joins
Alpine, both boot as the whole userspace, and devices reach `/sys` and udev the way they do on Linux.

**Unlocks.** Linux software as vibeOS's application layer: language runtimes, distribution package
managers, and the libudev, libinput, and libdrm device discovery that Era VII's desktop stack runs on.
Pass rates against Linux that anyone can reproduce from the pinned inputs.

**Architectures.** Both. Every suite runs on x86_64 and aarch64 in a 4-vCPU, 4 GiB guest on GitHub-hosted
runners: under KVM on x86_64, with one named `-cpu` model and `enforce` in every job, since the runners
draw AMD and Intel CPUs of several generations; under TCG on aarch64, since GitHub's arm64 runners have no
KVM. A case that passes on one architecture and fails on the other is a bug in the other. The §23.6
reference kernel runs each suite in the same guest shape, CPU model, and accelerator on the same runner
label, so a timing-sensitive case is compared like with like, and each gate share holds on every host CPU
model the x86_64 runner draws (§10.1). The kselftest `x86` target runs on x86_64 only, and arm64's
`signal` and `abi` targets on aarch64 only, since each tests its own architecture's entry and signal
paths.

**Exit gate**
- [ ] LTP's `syscalls` scenario, in full as §23.6 builds it, passes at least 80% of the cases that pass on the §23.6 reference kernel, on both architectures, and every other case is on its expected-failure list
- [ ] the kselftest targets `futex`, `rseq`, `pidfd`, `clone3`, `memfd`, `timers`, `sigaltstack`, `vDSO`, `proc`, `exec`, `mqueue`, and `mincore`, with `x86` on x86_64 and arm64's `signal` and `abi` on aarch64, pass at least 90% of the cases that pass on the reference kernel, on both architectures
- [ ] glibc's test suite, at the Debian root's glibc version, passes at least 95% of the tests that pass on the reference kernel, on both architectures
- [ ] CPython's regression suite (Debian's `python3` and its test-suite package) passes at least 95% of the test cases that pass on the reference kernel, on both architectures
- [ ] `go test std` with the pinned upstream Go release and `CGO_ENABLED=0` passes at least 95% of the tests that pass on the reference kernel, on both architectures
- [ ] Node.js (Debian's `nodejs`) passes at least 90% of the `test/parallel` cases from its version's source tarball that pass on the reference kernel, on both architectures
- [ ] OpenJDK (Debian's default JDK) completes every benchmark of the pinned DaCapo release, at its `default` size, that completes on the reference kernel, each passing DaCapo's own output validation, on both architectures
- [ ] an unmodified Alpine root boots as the whole userspace on both architectures (§23.5): busybox `init` as pid 1, starting OpenRC's `sysinit`, `boot`, and `default` runlevels from `/etc/inittab`, `mdev` populating `/dev`, networking through `udhcpc`, an `ssh` login from the host, and a clean `poweroff`
- [ ] an unmodified Debian root boots as the whole userspace on both architectures (§23.5): `sysvinit-core` as pid 1, `udev` populating `/dev`, networking through `ifupdown`, an `ssh` login from the host, and a clean `poweroff`; in it, `apt-get install build-essential` from the §23.2 mirror succeeds with `Release` signatures verified, and `dpkg --audit` reports nothing
- [ ] on the §23.3 test machine, `udevadm info --export-db` agrees with the reference kernel's on every device the machine's `-device` options add and on their children (subsystem, `DEVNAME`, `ID_PATH`, and the numbers of fixed-major devices); `libinput list-devices` finds the virtio keyboard and tablet; attaching a loop device and QEMU's `block_resize` each produce the kernel uevents the reference kernel produces; on both architectures
- [ ] the §23.4 shape check passes on both architectures
- [ ] tag `phase-23` and cut the next release

### 23.1 Surface
The Linux interfaces that glibc, Debian's tools, and the runtimes call and that Phases 13 to 17 do not
provide. Each lands with the semantics its LTP and kselftest cases check.

- [ ] `rseq`: registration per thread, `cpu_id` and `node_id` updated on every return to user after a migration, and a critical section aborted to its handler on preemption, migration, and signal delivery, with the `AT_RSEQ_FEATURE_SIZE` and `AT_RSEQ_ALIGN` auxv entries glibc reads; glibc registers every thread
- [ ] `clone3` over the same code as `clone`, taking every flag `clone` takes, plus `CLONE_PIDFD`, since glibc's `pthread_create` and `posix_spawn` try it first and fall back to `clone` only on `ENOSYS`
- [ ] pidfds: `pidfd_open`, `pidfd_send_signal`, `pidfd_getfd`, `waitid(P_PIDFD)`, and a pidfd that `ppoll` and `epoll` report readable when the process exits; Go's `os/exec` and Python use them; a pidfd refers to its process's id object, not the number, so once the process is reaped `pidfd_send_signal` returns `ESRCH`, even after the number is reused (DESIGN §2.11 rule 4)
- [ ] `close_range`, `copy_file_range`, `statx`, `faccessat2`, `openat2`, and `epoll_pwait2`, which glibc, coreutils, libuv, and Rust's `std` call before any fallback
- [ ] `membarrier` with `MEMBARRIER_CMD_PRIVATE_EXPEDITED` and its registration, over the §4.9 call-function IPI to the CPUs running the caller's address space
- [ ] `inotify` on vibefs and tmpfs with Linux's event layout, rename cookies, and `IN_Q_OVERFLOW`; `tail -f`, Node's `fs.watch`, and udev use it
- [ ] System V IPC: `shmget`, `shmat`, `shmdt`, `shmctl`, `semget`, `semop`, `semtimedop`, `semctl` with `SEM_UNDO`, `msgget`, `msgsnd`, `msgrcv`, and `msgctl`, with `/proc/sysvipc/*`; PostgreSQL still asks for a System V segment at startup; `CLONE_SYSVSEM`, which glibc's `pthread_create` passes, stops being §13.1's no-op: tasks created with it share one `SEM_UNDO` list, applied when the last of them exits
- [ ] POSIX message queues: `mq_open`, `mq_unlink`, `mq_timedsend`, `mq_timedreceive`, `mq_notify`, and `mq_getsetattr` over an `mqueue` filesystem; queue memory counts against `RLIMIT_MSGQUEUE` per real user ID, and an `mq_open` past it fails as mq_open(3) documents
- [ ] `fallocate` (with `FALLOC_FL_KEEP_SIZE` and `FALLOC_FL_PUNCH_HOLE`), `posix_fadvise`, `readahead`, `sync_file_range`, and `syncfs` on vibefs v2 and tmpfs, as PostgreSQL and `dpkg` call them
- [ ] `madvise` with Linux's semantics: `MADV_DONTNEED` zero-fills a private anonymous page on its next touch, `MADV_FREE` frees lazily, `MADV_DONTFORK` and `MADV_WIPEONFORK` apply at `fork`, and unknown advice returns `EINVAL`; Go's and the JVM's heaps depend on the first two
- [ ] `prctl` options that runtimes and init systems set: `PR_SET_NAME` and `PR_GET_NAME` (the `comm` files), `PR_SET_PDEATHSIG`, `PR_SET_CHILD_SUBREAPER`, `PR_SET_DUMPABLE`, and `PR_SET_TIMERSLACK` with `PR_GET_TIMERSLACK`, storing the per-thread value (built by whichever of §19.6 and §23.1 lands first) that §19.6's timer slack applies; any option no phase has landed on that architecture returns `EINVAL`, as an unknown option does on Linux, which callers such as `apt` read as an older kernel
- [ ] the smaller calls LTP and common tools reach: `kcmp`, `process_vm_writev` beside §17.4's `process_vm_readv`, `ioprio_get` and `ioprio_set`, `futex_waitv`, `personality`, and `mincore` with Linux's residency semantics over §12.2's demand paging and the §12.5 page cache
- [ ] the `release` in §13.10's `uname` policy tracks the §23.6 reference kernel's version, so LTP's and kselftest's minimum-kernel-version checks run on vibeOS the cases they run on the reference; the policy entry in `docs/LINUX.md` moves with the pin
- [ ] aarch64 SVE and SME: per-thread state saved and restored as §11.6's FP state is, with a thread's first SVE or SME instruction trapping once to size and set up its state, `prctl` `PR_SVE_SET_VL` and `PR_SME_SET_VL`, and the `sve_context` and `za_context` records in the §13.8 signal frame; `HWCAP_SVE` in `AT_HWCAP` and `HWCAP2_SME` in `AT_HWCAP2` appear only once this lands, so until then programs on the harness's `-cpu max` guests see neither and the SVE and SME cases of arm64's kselftest `signal` and `abi` targets stay on the expected-failure list naming this box
- [ ] aarch64: an EL0 `mrs` of an ID register (`MIDR_EL1`, `MPIDR_EL1`, `REVIDR_EL1`, and the `ID_AA64*` registers) traps and is emulated with the sanitized values Linux's arm64 cpu-feature-registers documentation defines, so each field describes only what the kernel supports on every CPU; `HWCAP_CPUID` appears in `AT_HWCAP` only once this lands. The instruction decoder and the sanitizing are host-tested, and a static program that prints each field runs in the §13.11 differential runner, with every field vibeOS reports differently from Linux listed in `docs/LINUX.md` with its reason. Until this lands, an EL0 `mrs` of an ID register gets `SIGILL` (DESIGN §11.4)

### 23.2 glibc and Debian
- [ ] glibc's dynamic linkers, `ld-linux-x86-64.so.2` through `/lib64` and `ld-linux-aarch64.so.1` in `/lib`, load through §14.2's `PT_INTERP` path unmodified
- [ ] the pinned Debian root (the `debuerreotype` build behind the official `debian` image) for each architecture, entered the way §14.9 enters Alpine's, with `bash`, coreutils, and `python3` running under glibc; `dpkg` and `apt`, like §14.9's `apk`, own files only inside a Linux root (a `vibeos-linux` chroot or a root booted whole, §23.5), never in the vibeOS base system
- [ ] glibc's startup, `pthread_create`, `posix_spawn`, multithreaded `setuid`, and thread cancellation paths traced once with `strace -f` over §17.4's `ptrace`, and every call they make implemented, none left to a fallback (F150)
- [ ] a pinned local Debian mirror snapshot on a disk image for each architecture, holding the closure of the packages this phase installs, read by `apt-get` through a `file:` source with `Release` signatures verified against Debian's archive keys by the verifier the pinned release's apt uses (`sqv`, or `gpgv`), so nothing here needs a route to the internet
- [ ] `dpkg` survives a power cut mid-unpack on §12.5's volatile-cache device, where a cut loses the writes the guest has not flushed: after the reboot, `dpkg --configure -a` completes and `dpkg --audit` reports nothing, which holds only if vibefs honors the `fsync` and `rename` ordering `dpkg` relies on; the cut falls at a seeded random point in each of 100 unpacks of one package set, under TCG with 2 vCPUs and 2 GiB, on both architectures; a killed QEMU loses no write the host received (§12.5), so killing QEMU cannot show a missing flush (F080)

### 23.3 Devices in Linux's layout
This replaces §8.4's `sysfs`-equivalent layout. libudev, `mdev`, libinput, libdrm, `lsblk`, and `ip` find
devices here.

- [ ] `/sys/devices` holds every device the kernel enumerates, bound to a driver or not, at Linux's paths (`pci0000:00/0000:00:03.0/virtio0/...`, and on aarch64 the device-tree platform bus named as Linux names it, `<address>.<node>`); `/sys/bus/{pci,virtio,platform}/{devices,drivers}`, `/sys/class/{block,net,input,drm,tty,mem,misc}`, and `/sys/dev/{block,char}/<major>:<minor>` link into it
- [ ] each device's `uevent`, `dev`, `subsystem`, and `driver` entries, and the bus attributes udev reads (`vendor`, `device`, `class`, `subsystem_vendor`, `subsystem_device`, `modalias`)
- [ ] block devices with `size`, `ro`, `removable`, `queue/logical_block_size`, and, per partition, `partition` and `start`, which `lsblk` and udev read; `BLKRRPART` (built by whichever of §22.2 and §23.3 lands first, with §22.2's `EBUSY` rule) rescans a disk's partitions with their uevents; a virtio-blk capacity change (QEMU's `block_resize`) updates `size` and sends `change` with `RESIZE=1`
- [ ] network interfaces under `/sys/class/net` with `address`, `mtu`, `operstate`, `carrier`, `ifindex`, `type`, and `statistics/`, which `ip`, `ifupdown`, and udev's interface naming read
- [ ] input devices under `/sys/class/input` with Linux's `capabilities/` bitmaps and `id/` files over §16.4's evdev nodes, since udev's `input_id` classifies keyboards and pointers from them
- [ ] the display under `/sys/class/drm` (`card0` and its connectors) over §16.1's node, so libdrm enumerates it
- [ ] device numbers are Linux's: the fixed majors in Linux's `devices.txt` (`mem` 1, `tty` 4 and 5, loop 7, `input` 13, `pts` 136, `ttyAMA` 204, `drm` 226) and dynamic majors for the rest, in `st_rdev`, the `dev` files, and `/proc/devices`; TTYs named as Linux's drivers name them, `ttyS0` for the 16550 and `ttyAMA0` for the PL011
- [ ] §16.1's `NETLINK_KOBJECT_UEVENT` socket carries every device's events: the kernel multicasts `add`, `remove`, `change`, `move`, `bind`, and `unbind` to group 1 in Linux's format (an `ACTION@DEVPATH` header, then `ACTION`, `DEVPATH`, `SUBSYSTEM`, `SEQNUM`, and the device's own keys), with `SCM_CREDENTIALS` and a sender `nl_pid` of 0, which libudev checks; `SO_ATTACH_FILTER` (§15.7's classic BPF) on these sockets, since libudev filters in the kernel; user processes multicast to group 2, as `udevd` does to libudev monitors
- [ ] writing an action to a device's `uevent` file replays that event, which `udevadm trigger` depends on; `/sys/kernel/uevent_seqnum` tracks `SEQNUM`
- [ ] loop devices: `/dev/loop-control` with `LOOP_CTL_GET_FREE`, and `LOOP_CONFIGURE`, `LOOP_SET_FD`, `LOOP_CLR_FD`, and `LOOP_SET_STATUS64` with partition scanning, so `losetup` and `mount -o loop` work; attach and detach send their uevents
- [ ] `/dev/rtc0` with `RTC_RD_TIME` and `RTC_SET_TIME` over the §2.7 RTC and the PL031 (§11.5), for `hwclock` at boot
- [ ] `/sys/devices/system/cpu/{online,possible,present}`, each CPU's `topology/`, and `/sys/kernel/mm/transparent_hugepage/{enabled,hpage_pmd_size}` in Linux's formats, which glibc's `get_nprocs`, `lscpu`, Go, and the JVM read
- [ ] the test machine: one QEMU command line per architecture adding two virtio-blk disks, virtio-net, virtio-gpu, virtio-keyboard, and virtio-tablet, booted by vibeOS and by the reference kernel over the same Debian root; the gate compares every device those `-device` options add, and their children

### 23.4 /proc in Linux's formats
This extends §13.9's procfs, §13.10's `/proc/self` pieces, §14.9's system-wide files, and §17.1's build-system
entries to what procps, util-linux, glibc, and the runtimes parse.

- [ ] per process: `statm`, `smaps` (which pins the address space as §13.9's `maps` does, and whose Pss needs a per-unit map count, which this line adds to §12.1's unit within its 64-byte budget), `limits`, `io`, `mounts`, `mountinfo`, `fdinfo/`, `cgroup`, which reports the root group (`0::/`) until §21.5's cgroup v2, and `oom_score_adj`, stored and read back as on Linux, which §19.10's OOM score uses; `task/<tid>/` carries each per-pid file for its thread
- [ ] system-wide: `vmstat`, `cmdline`, `partitions`, `devices`, `diskstats`, and `swaps`
- [ ] `/proc/sys`: `kernel/{domainname,threads-max,printk,random/uuid,core_pattern,dmesg_restrict}`, `vm/{max_map_count,min_free_kbytes,overcommit_ratio,overcommit_kbytes}`, `fs/{file-max,nr_open,inotify/*}`, `net/core/{somaxconn,rmem_default,rmem_max,wmem_default,wmem_max,netdev_max_backlog}`, `net/ipv4/{tcp_mem,tcp_rmem,tcp_wmem,udp_mem}`, and `net/ipv4/neigh/default/{unres_qlen_bytes,gc_thresh3}` beside §14.9's entries, all writable by root where Linux allows it, so `sysctl -w` works; `core_pattern` holds the pattern §13.8's command-line option sets at boot, and `dmesg_restrict` the §13.9 rule, both writable only by root; a write to `vm/min_free_kbytes` resizes §12.6's reserve; writing 1 or 2 to §14.9's `vm/overcommit_memory` selects Linux's always-succeed and strict modes over §12.4's commit count, and the strict mode refuses a request that would take the count past `CommitLimit` (swap plus RAM times `overcommit_ratio` over 100, or `overcommit_kbytes`), which `/proc/meminfo` shows; an in-guest test repeats §12.4's 2 GiB request in each mode
- [ ] the process, thread, and open-file tables, each process's descriptor table, and each address space's regions grow on demand instead of being allocated whole at init (§10.4), up to `kernel/threads-max` (processes and threads together, as on Linux), `fs/file-max`, `fs/nr_open`, and `vm/max_map_count`, whose boot values are Linux's (`nr_open` 1,048,576, `max_map_count` 65,530, and `file-max` and `threads-max` computed from memory); every structure §10.4 sizes from the thread count, the wake inbox and the KVA free-list pool among them, grows with the thread table, so no wake is dropped; `fs/nr_open` replaces §13.9's `limits` constant as the `RLIMIT_NOFILE` ceiling, and `setrlimit` above it returns `EPERM`; exhaustion returns Linux's errors (`EMFILE`, `ENFILE`, `EAGAIN` from `fork` and `clone`, `ENOMEM` from `mmap`); an in-guest test raises `fs.nr_open` and `RLIMIT_NOFILE` and opens 4,096 descriptors in one process, maps 1,024 non-adjacent regions in one address space, and lowers `fs.file-max` below the open-file count, after which an unprivileged process's `open` returns `ENFILE`
- [ ] §14.9's formatter host tests extended to every file this subsection names, against captures from the §23.6 reference kernel
- [ ] the shape check: an in-guest script reads every `/proc` file this subsection and §14.9 name and every `/sys` attribute §23.3 names for the test machine's devices, replaces each number, run of numbers, and address with a placeholder, and diffs the result against the same normalization of a capture from the reference kernel on the §23.3 test machine; captures checked in and retaken when a pin changes
- [ ] procps-ng's `sysctl` and `vmstat` and util-linux's `lscpu`, `lsblk`, `findmnt`, and `mount` from the Debian root run unmodified, and Debian's `ps -e` lists the processes the native `ps` lists

### 23.5 Distributions booted whole
- [ ] a distribution root boots without an initrd: the kernel mounts the vibefs v2 disk that §14.8's `root=` names, read-only or read-write as `ro` or `rw` says, mounts `devtmpfs` on its `/dev` as Linux's `CONFIG_DEVTMPFS_MOUNT` does, and runs its `/sbin/init`, or the program `init=` names, as pid 1 with its standard descriptors on `/dev/console`; `ro`, `rw`, and `init=` join the §10.2 parser here; vibeOS loads no modules, so no distribution initramfs is used, and `modprobe`'s failures are a deliberate gap in `docs/LINUX.md`
- [ ] the pseudo filesystems mount under Linux's type names and options, as init scripts mount them: `devtmpfs` (writable, so `udev` and `mdev` create nodes and links in it), `sysfs`, `proc`, `devpts` with `gid`, `mode`, and `ptmxmode`, `tmpfs` with `mode`, `size`, and `nr_inodes`, and `mqueue`
- [ ] `mount` honors `MS_REMOUNT` (root from read-only to read-write), `MS_BIND`, `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV`, and `MS_NOEXEC`
- [ ] the calls init and getty make: `reboot` with Linux's magic numbers and commands, including the ctrl-alt-del ones every init sends; `vhangup`; and `syslog(2)` for `klogd` and busybox `dmesg`, whose reads follow §13.9's `dmesg_restrict` rule
- [ ] Alpine: a root that `apk` installs from the §14.9 mirror into an empty directory, as `setup-disk` does (`alpine-base`, OpenRC, busybox `mdev`, `openssh`), written to a vibefs v2 disk; the build writes configuration files (`fstab`, `inittab`, `interfaces`) and changes no binary
- [ ] Debian: the §23.2 root after `apt-get install sysvinit-core udev ifupdown openssh-server` from the mirror, written to a vibefs v2 disk, with configuration written the same way
- [ ] the harness boots each root, waits for its serial login prompt, logs in over `ssh` through QEMU's user networking, writes a file and runs a command, and ends with the root's own `poweroff`; the run passes only when QEMU exits on the guest's power-off request, host `fsck-vibefs` then finds the root disk clean, and the file is on the disk with its contents, so the shutdown path's sync and unmount are tested as well (F060)

### 23.6 Suites
- [ ] the reference kernel: Debian's `linux-image` from the §23.2 mirror with its own initramfs, booted on the same QEMU machine, CPU model, accelerator, CPU count, and memory as the vibeOS guest, on the same hosted runner label, over the same root contents on ext4, with each suite's scratch directory on tmpfs in both; a case counts as passing on the reference when it passes in each of three runs, and the passing sets are checked in and retaken when a pin changes
- [ ] LTP at a pinned release, built as §13.11 builds its static corpus, running every case of the `syscalls` scenario rather than §13.11's subset, except the cases LTP's own `ci/alpine.sh` removes at that release as not building against musl; those fail on the reference kernel too, so they are outside its passing set and need no expected-failure entry
- [ ] the kselftest targets the gate names, from the reference kernel's Linux release, built on the host as test inputs and never shipped
- [ ] glibc's test suite, built on the host for the Debian root's glibc version, each test run in the guest through glibc's `test-wrapper` hook over the §17.4 host mount
- [ ] each runtime's suite run by its own runner with per-case results: `python3 -m test -j4 --junit-xml`, `go test -json std`, Node's `tools/test.py` against the packaged binary, and DaCapo's harness under the packaged `java` at the `default` size, from the pinned release's `-minimal` distribution, which holds every benchmark with the inputs its `default` size reads (1.8 GB extracted at 23.11-MR2-chopin, against 16 GB for the full one), extracted once on the host and read by both kernels over the §17.4 host mount, as glibc's tests are
- [ ] one expected-failure list per suite and architecture, extending §13.11's format with a class: each entry names the case, its class (a kernel bug with an issue link, a missing feature with the line in this file that lands it, or a deliberate gap with its `docs/LINUX.md` entry), and a reason; `scripts/check_expected_failures.py` in `make check` fails on an entry without all three
- [ ] the runner fails on a case from the reference passing set that fails and is not listed, on a listed case that passes, as §13.11's runner does, so the lists only shrink, and on a suite whose pass share falls below its gate line; the share leaves out a listed case that names a line in Phases 18 to 22 until that phase's `phase-<N>` tag exists
- [ ] the suites on a weekly scheduled job, in shards of at most 5.5 hours (§10.1), since a GitHub-hosted job stops at 6, with per-suite and per-architecture counts, and each x86_64 shard's host CPU model, in the job summary and in §10.9's CI history, which records this workflow's runs beside `ci`'s, with the counts as added fields
- [ ] `docs/LINUX.md` carries a section per suite, generated from the lists and the reference sets: cases passing on the reference and on vibeOS per architecture, and every deliberate gap with its reason (loadable modules and 32-bit entry points among them); `make check` fails when it is stale

---

## Phase 24: Source Bootstrap and Ports

**Goal.** Nothing in the image that builds vibeOS is an upstream binary taken on trust. The toolchains
Phase 17 borrowed through §17.7 are rebuilt from source on vibeOS, reach a fixed point, and are checked by
diverse double-compiling; the seeds that remain are listed and confined to stage 0. A ports tree turns the
same machinery into a package collection with a size target.

**Unlocks.** A self-hosting claim with no borrowed binaries past a short, listed seed set. Packages vibeOS
builds instead of downloading, which Era VII's desktop stack and Phase 39's supported releases build on. Rust's
`std` built from source for the Linux triples vibeOS runs (§24.3).

**Architectures.** Both. Each architecture rebuilds its own toolchains and ports natively on vibeOS, in a
build guest on a GitHub-hosted runner where the §22.4 CI agent runs the builds (on aarch64 through
§24.1's job): x86_64 under KVM, aarch64
under TCG, since GitHub's arm64 runners have no KVM. The build guest has 4 vCPUs and 6 GiB, which leaves the
runner's 16 GB room for QEMU, the host, and the page cache over the shards' disk images, and keeps its build
trees on virtio-blk disk images. A hosted job stops at 6 hours, so a longer build runs as a chain of §24.1
shards of at most 5.5 hours that carry the disk images from job to job as artifacts. The 14 GB disk
GitHub documents for a hosted runner is less than the 30 GB the rustc-dev-guide asks for a `rustc`
build, so each shard first frees space as rust-lang's CI does, deleting the runner image's preinstalled
SDKs and tool caches and keeping the images on `/mnt` where the runner mounts one with more room, and
writes the free space it measured to its job summary; DESIGN §8.6 records, per runner label, the free
space after cleanup and each build's peak image size. If a build's recorded peak exceeds that space on
its architecture's runner, an edit to this file moves the gate lines that need it, for that
architecture, to the [funded goal](#funded-goals) buying that architecture's test machine, which
rebuilds bare metal without shards, as Phase 21 moves its nested lines. TCG runs aarch64's
builds several times slower, so aarch64's full rebuild runs monthly and x86_64's weekly (§24.2). Go's
bootstrap on aarch64 starts from a toolchain cross-built from source on x86_64 vibeOS and carried over
as an artifact, since Go 1.4 has no arm64 port; nothing else crosses architectures.

**Exit gate**
- [ ] `tcc`, LLVM with clang and lld, `rustc` and `cargo` at the pinned nightly, CPython, QEMU, `git`, `make`, `cmake`, `ninja`, `xorriso`, `nasm`, and Limine are rebuilt from source natively on vibeOS, starting from §17.7's upstream toolchains, on both architectures (§24.1)
- [ ] `tcc`, clang with lld, and `rustc` each reach a fixed point on both architectures: rebuilt by its own from-source build, the result is byte-identical (§24.1)
- [ ] the ISO that the §24.1 toolchains build and the ISO that §17.7's upstream toolchains build have byte-identical kernel ELFs and initrds, and every other differing file is listed with its cause in `docs/BOOTSTRAP.md`, which the §17.5 loop script checks, on both architectures (§24.1)
- [ ] diverse double-compiling: stage-1 clang and lld built from one source by Alpine's clang and by Debian's gcc (Phase 23) build byte-identical stage-2 toolchains, on both architectures (§24.5)
- [ ] the chain from stage 0 to the build image runs in roots holding only the seeds `docs/BOOTSTRAP.md` lists or the previous stage's output, and `scripts/check_bootstrap.py`, run by the §24.2 full rebuild over the stage manifests, finds no seed and no unlisted binary in the final build image, on both architectures (§24.5)
- [ ] at least 500 ports build natively into §14.6 packages signed with §24.2's rebuild key on both architectures in one §24.2 full rebuild per architecture, each with its own test suite passing except the cases on its reasoned expected-failure list
- [ ] at least 95% of the ports reproduce: built twice from the same commit, their packages are byte-identical
- [ ] CPython, Go, Node.js, and OpenJDK built as ports pass their §23.6 suites at Phase 23's thresholds, measured against the passing sets of the same port builds run under the §23.6 reference kernel as §24.2 runs them, on both architectures
- [ ] no port carries a vibeOS-specific source patch that names no `docs/LINUX.md` divergence, and none carries a patch against musl, Limine, or QEMU
- [ ] `library/std`'s test suite for the §24.3 host triple passes natively on both architectures, except the cases on its reasoned expected-failure list
- [ ] tag `phase-24` and cut the next release

### 24.1 Toolchain from source
Moved from §17.2 and §17.3, which close Phase 17 on upstream binaries (§17.7). Each rebuild starts from
§17.7's upstream toolchains and runs natively on vibeOS.

- [ ] the §22.4 CI agent's aarch64 job: §22.4's workflow job also runs on the hosted arm64 runner, booting an aarch64 image from the §22.2 unattended installer under TCG, with the same agent taking the commit and posting its status, so each architecture's build guest is a vibeOS install the agent drives
- [ ] shards: a hosted job boots the build guest on the disk images the previous shard of its chain uploaded as artifacts, lets the §22.4 CI agent run the build, and stops it with `SIGINT` in time to shut the guest down cleanly and upload the images before the job's 5.5-hour mark, so the next shard resumes from the tree it left; every shard of a chain runs the same QEMU and `-cpu` model, on x86_64 a named model with `enforce`, since hosted x86_64 runners draw AMD and Intel CPUs of several generations; each build keeps its tree on a disk image of its own, dropped rather than uploaded once its outputs are copied out, so a shard carries the tree of the build in progress and only the outputs of finished ones; a shard fails before booting, naming the sizes, when its images would not fit the free space left after cleanup; a harness test stops a small build at a random point and checks that the resumed build's output is byte-identical to an uninterrupted one's
- [ ] `tcc` first: small, self-hosting, and a fast way to prove the from-source path; built by §17.7's clang, then by itself until two generations match
- [ ] GNU binutils from source, for the ports that call `as` and `ld` by name
- [ ] then LLVM with clang, lld, compiler-rt, libunwind, and libc++, built first by §17.7's clang and then by itself until two generations of clang and lld match byte for byte, each a Release build without debug info; mostly a test of the C++ standard library and of filesystem behavior at scale
- [ ] `rustc` and `cargo` through `x.py` at the pinned nightly, since the kernel uses nightly features: natively, as a local rebuild with §17.7's `rustc` and `cargo` as stage 0, and with rustc's own `src/llvm-project`, so its code generation matches the upstream build's; then again, as a local rebuild with that build's `rustc` and `cargo` as stage 0, until two generations of `rustc` match byte for byte
- [ ] the `x.py` build's targets are the host triple, both kernel targets, and every target the ISO's user programs build for, so the `core`, `alloc`, and `compiler_builtins` the kernel links come from this build and not from upstream's `rust-std`, which the kernel links today (no `build-std`)
- [ ] the `x.py` build's debug info is off for the compiler, the tools, and the `src/llvm-project` LLVM, while the target libraries keep upstream's dist settings, `rust.remap-debuginfo` included, so their source paths read `/rustc/<commit>` as upstream's do; the kernel, built with `debug = true`, carries their debug info and panic locations into the ELFs the §17.5 loop script compares (F151)
- [ ] CPython, QEMU, and `xorriso` (§17.6), `git` (§17.4), `make`, `cmake`, and `ninja` (§17.2), and `nasm`, which Limine's x86 build needs, rebuilt the same way
- [ ] Limine from its pinned source release, built with the rebuilt clang and `nasm`, in place of the binary branch `setup.sh` fetches, in the ISO the §24.1 toolchains build
- [ ] the §17.5 loop script builds the ISO once with §17.7's upstream toolchains in the `vibeos-build` image and once with the §24.1 toolchains in the §24.5 build image, and fails when the kernel ELFs or initrds differ, or when another file differs without its cause in `docs/BOOTSTRAP.md`, on both architectures; the two toolchains sit at different install paths, which §10.2's path remapping keeps out of the kernel image (F151)
- [ ] build times recorded next to §17.5's, per architecture with its accelerator (x86_64 under KVM, aarch64 under TCG), as each shard chain's summed job time and wall time, with the §24.2 full rebuild's beside them

### 24.2 Ports
Ports are §14.6 packages, built by §14.6's recipe tool like the base system and everything a release
ships. Scheduled rebuilds sign them with the rebuild key in `tests/keys/`, whose public half only the
§14.3 test overlay trusts, since only `release.yml`'s `sign` job may hold the release key (§14.6); for
each port a release ships, the release's `build` job takes it from the rebuild's runs and refuses one
whose two builds (below) disagree, and the `sign` job re-signs it into the release repository with the
release key, and §14.6's test-anchor check refuses a shipped package still signed by the rebuild key.

- [ ] the recipe source decided on measured yield and written in `docs/PORTS.md`: an importer that turns Alpine's APKBUILDs (musl, both architectures) into §14.6 recipes, pkgsrc (built for new platforms), or recipes written by hand; each tried on the same 50 packages, with the counts recorded
- [ ] the ports tree grows in §14.10's `ports/` layout, and `make check` fails on a port with no §14.6 recipe
- [ ] the §24.1 toolchains are ports built by the same machinery, and the tree holds the build closure of each of them and of the Phase 23 runtimes
- [ ] hermetic builds: each port builds natively in a fresh root holding only its declared build dependencies, from sources fetched and hash-checked beforehand, with no network during the build
- [ ] each port's own test suite runs after its build; a failure is fixed, or listed in the port's expected-failure list with a class and a reason, which `make check` requires, as it does for §23.6's lists
- [ ] a port whose build and test suite pass in the same build root run under the §23.6 reference kernel (entered with `chroot` from its Debian root), and fail on vibeOS, is a kernel bug or a missing feature, not a port patch; a vibeOS-specific source patch is allowed only for a divergence `docs/LINUX.md` lists, and `make check` fails on one that names none
- [ ] every port built twice from the same commit gives byte-identical packages, or is listed with its cause and kept out of the release repository until it does, so §22.1's reproducibility holds for everything a release ships; the full rebuild builds each port twice, running its test suite in one of them, in separate shard chains on separate runners whose shard boundaries fall at different points, so neither the runner's CPU nor a resumed build reaches a package unnoticed
- [ ] Go through upstream's bootstrap chain from Go 1.4's C sources, so Go adds no seed; on aarch64 from a bootstrap toolchain cross-built on x86_64 vibeOS
- [ ] a port whose build needs a binary of itself (a boot JDK for OpenJDK) lists that binary as a port seed in `docs/BOOTSTRAP.md`
- [ ] the full rebuild: a scheduled workflow rebuilds the whole tree from §24.5's stage 0 through the §22.4 CI agent in §24.1 shard chains, weekly on x86_64 and monthly on aarch64, whose TCG builds run several times slower; a nightly job, one run at a time per architecture, rebuilds the ports whose recipe or dependencies changed since its last complete run; the rebuild workflows' peak job counts, recorded in DESIGN §8.6, sum to at most 5, their share of §10.1's 10 scheduled slots, so a rebuild that runs for days leaves the other 5 to the nightly and weekly workflows and per-push CI its own 10; when §24.1's recorded chain times show a full rebuild would not finish within its period at that share, the period lengthens and the share stays, and a rebuild that would outlast GitHub's 35-day limit on a workflow run continues in a run it dispatches; per-architecture counts (ports, built, test suites passing, reproducible, carried patches) go to the job summary and to §10.9's CI history, which records these workflows' runs beside `ci`'s, with the counts as added fields

### 24.3 Rust target
Moved from §17.3. Decided: vibeOS's Rust host and user triple is `<arch>-unknown-linux-musl`, and
`<arch>-unknown-linux-gnu` inside Phase 23's glibc roots; no `*-unknown-vibeos` triple exists. `std`'s
Linux layer is correct on vibeOS by the Linux-interfaces rule, and SYSCALL.md §8 keeps vibeOS-only
interfaces out of the syscall table, so a separate triple would buy only a distinct `target_os`, for a
patch set rebased at every toolchain bump and a port of the `libc` crate. `docs/LINUX.md` records the
decision; it is revisited only if `std` itself must call a vibeOS-only interface.

- [ ] `library/std` for `<arch>-unknown-linux-musl`, with thread, file, socket, process, and time support, built natively by §24.1's `x.py` build with no vibeOS patch
- [ ] `x.py test library/std` for that triple passes natively on both architectures, except the cases on its expected-failure list, each with a reason

### 24.4 Upstream
- [ ] every carried patch under `ports/` records its upstream status: merged in a named release, filed with a link, or declined with a reason; `make check` fails on a patch without one
- [ ] no carried patch against musl, Limine, or QEMU, the projects vibeOS's own build and harness run on; a patch there is upstreamed or made unnecessary by a kernel fix
- [ ] carried patches counted per port by the §24.2 full rebuild, in its record on §10.9's `ci-history` branch

### 24.5 Seeds and diverse double-compiling
Bootstrappable builds: every binary the chain starts from is named, confined to stage 0, and cross-checked.

- [ ] `docs/BOOTSTRAP.md` lists every seed, a binary the chain uses but did not build, with its origin, version, and SHA-256, toolchain seeds and §24.2's port seeds separately; `scripts/check_bootstrap.py` in `make check` fails on a seed missing any of the three
- [ ] the toolchain seeds are Alpine's clang and lld with the libraries they load, the pinned `rustc` and `cargo`, Alpine's static `busybox` for `awk` and the other POSIX utilities `configure` scripts call that §14.4's base system lacks, and the vibeOS base system; stage 0 runs in a root holding only them, so an unlisted binary cannot be used by accident
- [ ] stage 0 rebuilds what build systems need before anything uses it: GNU make through its `configure` and `build.sh`, then `busybox` from source in place of the seed, then Perl, CPython, CMake, and ninja
- [ ] the §24.2 full rebuild runs the chain from stage 0, each stage a §24.1 shard chain of its own; every later stage runs in a root holding only the previous stage's output, and the build image is the last stage alone; each stage writes a manifest of its root (path, SHA-256, producing stage) kept as an artifact of the workflow run, and `scripts/check_bootstrap.py` in that run checks each root against the previous stage's manifest and fails on a seed or an unlisted binary in the build image
- [ ] diverse double-compiling for clang and lld: stage 1 is built from one source both by Alpine's clang and by Debian's gcc (a check-only seed, run in the Debian root and linking its stage 1 statically so it runs in the stage root); each stage 1 builds stage 2 in the same root, and the two stage-2 toolchains are byte-identical; a difference is a nondeterminism bug, fixed here or reported upstream

### 24.6 Stretch: Rust without a binary seed
- [ ] `rustc` from mrustc on vibeOS: mrustc builds the newest `rustc` release it supports, each release builds the next up to the pinned nightly, and the result is byte-identical to the §24.1 `rustc`, which removes the Rust seed and double-compiles `rustc` diversely

---

# Era VI. Production

From an operating system someone can install to one that runs unattended and at scale, in the virtual
machines that servers and clouds run. Every comparison with Linux runs the same workload in the same
guest shape, with the same devices, on the same runner and in the same job where it can, with Linux
booted in vibeOS's place, so the runner's noise falls on both.

Phase 25 needs 22, whose panic policy and watchdog it extends. Phase 26 needs 25 for SMBIOS and the
kexec entry. Phase 27 needs only 18, 19, and 20, so it can start beside 21 and 22. Phase 28 needs 25
for the AER hook, 27 for per-CPU vectors, and 23 for the `/sys/bus/pci` layout that QEMU's `vfio-pci`
finds devices through and the growable descriptor and file tables (§23.4) its 100,000-connection gate
needs. Phase 29 needs 18, 19, 20, and 23, whose surface `fio` runs on. Phase 30 needs 23 and 25 and
nothing from 26 to 29: [Phase 39](#phase-39-stability) needs it, and 1.0 does not wait for cloud VMMs,
thousand-CPU guests, NIC virtual functions, or disk arrays.

**Hosts.** Every line here runs on the free resources [How to read this](#how-to-read-this) lists. "The
KVM runner" is the hosted x86_64 runner with `/dev/kvm` that the §10.1 KVM leg runs on, and a fixed
threshold under KVM holds on every CPU model the runner draws, by that leg's rule, while a ratio against
Linux compares the two kernels in one job, on one CPU model. GitHub's arm64 runners have no KVM, so
aarch64 guests there run under TCG, and an aarch64 number that needs an accelerator is taken under HVF
on the dev host as a §10.9 record, where the peers, servers, and load generators a line puts on the
runner run in a Linux baseline guest on the dev host. Both kernels' guests reach that guest through
QEMU's `stream` netdev, a socket between the two QEMU processes that needs no root: macOS has no tap
device, and QEMU's `vmnet-*` netdevs and `socket_vmnet` need a root service, which no line installs on
the owner's Mac (How to read this). A guest larger than a runner, with hundreds of vCPUs or a
terabyte of memory, runs under TCG on sparse host memory and checks correctness and counts, not speed.
Real servers, clouds, 100GbE NICs, data-center drives, and month-long uptime are
[Funded goals](#funded-goals); no line here waits for one.

**Nested virtualization.** A line that runs vibeOS as a hypervisor inside the runner's guest runs in the
environments [Phase 21](#phase-21-virtualization) defines and passes on its terms: on x86_64 the nested
job, which GitHub calls experimental and whose runners draw AMD's SVM or Intel's VMX at random, so the
line needs, within the nightly job's last 7 runs, a passing leg of each path that §20.8's records of
those nights show a runner offering; on aarch64 the EL2 job, under TCG with `virtualization=on`, which
records its numbers without a threshold, and the HVF record, on QEMU 11.1 or later. If those records
show no runner offering either path for 7 nights running, GitHub has withdrawn nesting, and the edit
Phase 21 then makes also moves the x86_64 parts of Phase 28's VF-assignment line and Phase 30's
live-update line to the x86_64 test PC in [Funded goals](#funded-goals); each then closes on its
aarch64 parts.

**Long runs.** A run longer than one hosted job is a chain of shards of at most 5.5 hours that carry
their state as artifacts, by the rule in [How to read this](#how-to-read-this) and §10.1's CI budget.
Here each shard ends by saving the guest with QEMU's `migrate` to a file and uploads the stream, the
disk images, and the counter series; the next shard restores them with `-incoming`, so the guest kernel
keeps running from one shard to the next (§25.7). A carried guest runs under TCG, since a KVM guest's
state does not move between the runners' Intel and AMD hosts, and a chain holds one hosted job at a
time.

## Phase 25: Reliability

**Goal.** A system that reports its own hardware errors, survives the ones that can be survived,
notices when it has hung, and leaves a dump when it dies, with nobody at the console.

**Unlocks.** Running unattended, which Phase 26's cloud images need because nobody can reach their
consoles. Debugging a crash from its dump rather than from a reproduction. The soaks every later phase
leans on.

**Architectures.** Both. Machine checks are x86_64: MCA banks and CMCI. The aarch64 equivalents are
SError, synchronous external aborts, and the RAS extension's error records. Both report platform errors
through APEI on ACPI machines. The QEMU monitor's `mce` command injects x86 machine checks under TCG and
KVM, so §25.1 and §25.3 are gated under QEMU on x86_64; QEMU models no CMCI, so corrected errors are
found by polling there. On aarch64, QEMU from 10.2 injects a CPER record into a GHESv2 error source with
its unstable QMP command `inject-ghes-v2-error`, on `-machine virt,ras=on` booted with ACPI, so the
aarch64 CPER decode and §25.3's poisoned-page handling are gated under QEMU through §20.7's ACPI path,
and QEMU raises a synchronous external abort for an access that no device or memory answers. QEMU
builds no EINJ table and no GHES on `q35`, raises no SError, and gives `-cpu max` no RAS error records
(`ERRIDR_EL1` reads 0), so those parsers and decoders are host-tested, against the server reports of the
linuxhw/ACPI corpus and recorded register values. QEMU builds an ERST table only on x86
(`-device acpi-erst`), so the ERST backend is gated on x86_64, and aarch64 keeps its panic record in an
EFI variable. PCIe AER, kexec, crash capture, watchdogs, and persistent records are shared and gated
under QEMU on both. The hard lockup detector's NMI comes from the emulated PMUv3 on aarch64 under TCG;
x86 TCG emulates no PMU and no free host is known to give a guest one (Phase 19), so x86_64 uses the
buddy check. CMCI, the x86_64 PMU NMI, EINJ and firmware-first reporting on real firmware, and a
continuous soak on real machines are [Funded goals](#funded-goals).

**Exit gate**
- [ ] a corrected memory error injected with the QEMU monitor's `mce` command is logged with its bank, address, and severity and counted per bank; an uncorrected action-required error in a user page mapped by two processes retires the frame and sends `SIGBUS` with `BUS_MCEERR_AR` to the process that consumed it, and to the other when it next touches the page, and the kernel keeps running; x86_64 under TCG with an Intel CPU model (`-cpu Skylake-Client-v4`, since TCG's `-cpu max` is AMD) and under KVM on the KVM runner, where the harness injects the status encoding of the job's CPU vendor; 4 CPUs, 2 GiB
- [ ] an uncorrectable error injected with QEMU's `pcie_aer_inject_error` into a virtio-net device (`aer=on`) behind a PCIe root port is logged with the device, the link is reset, the driver's `error_detected` hook runs, and the device carries traffic again without a reboot, on both architectures (x86_64 in §20.9's `q35` configuration)
- [ ] under QEMU 10.2 or later on aarch64 `-machine virt,ras=on` booted with ACPI (TCG, 2 CPUs, 1 GiB), a corrected memory-error CPER record injected with `inject-ghes-v2-error` is decoded and logged with its address, its severity, and the locator of the DIMM whose SMBIOS type 17 handle it names, and a recoverable uncorrected one naming a user page retires the frame and sends `SIGBUS` with `BUS_MCEERR_AR` to the process when it next touches the page, and the kernel keeps running
- [ ] §25.2's host tests parse the HEST, BERT, ERST, and EINJ tables of every server report in the linuxhw/ACPI corpus that carries them, each rebuilt from its raw table bytes
- [ ] a panic under QEMU with a capture kernel loaded boots the capture kernel through kexec without firmware; it writes a filtered ELF vmcore, and host `gdb` opens it with the kernel ELF and prints the panicking CPU's backtrace and the registers of a second CPU that was spinning with interrupts off at the panic; both architectures under TCG, 4 CPUs, 2 GiB, aarch64 on `virt,gic-version=3`
- [ ] `kexec` from a running system reaches `shell ready` in the new kernel without firmware in under 2 s, in a 2-vCPU, 1 GiB guest under KVM on the KVM runner, and on aarch64 under HVF on the dev host as a §10.9 record
- [ ] a CPU spinning with interrupts off for 10 s is reported by the hard lockup detector with a backtrace whose first two frames the harness matches, through the kernel ELF's symbol table, to the test's spin function and its caller: through GICv3 pseudo-NMIs raised by the emulated PMUv3's overflow interrupt on aarch64 under TCG, and through the buddy check on x86_64 under TCG and under KVM on the KVM runner; on x86_64, a kernel thread on another CPU exits during the spin, and the shootdown its stack free sends waits in `wait_acks` until the spinning CPU acknowledges it after the spin, with no CPU panicking, as §10.10's `wait_acks` box requires (F070, F084)
- [ ] with a test hook in the `kernel_tests` build stopping one CPU's scheduler from switching threads for 30 s with interrupts on, the soft lockup detector reports that CPU and its backtrace, on both architectures under TCG
- [ ] after §20.6's `i6300esb` watchdog, armed by §22.2, resets a guest whose CPUs all spin with interrupts off (lockup detectors off), the next boot reports the reset reason as watchdog, and `WDIOC_GETBOOTSTATUS` returns `WDIOF_CARDRESET`, under QEMU on both architectures
- [ ] after a panic and a cold restart (under QEMU, a new QEMU process on the same variable store), the next boot logs the panic record: from an EFI variable under OVMF and the aarch64 edk2 build, and from ERST on x86_64 `q35` through QEMU's `acpi-erst` device, backed by a file the new QEMU process reopens
- [ ] a guest with no serial port (`-serial none`) reports its panic through netconsole to the harness's listener, on both architectures
- [ ] the §25.7 SQLite test, in WAL mode with `synchronous=FULL` on §12.5's volatile-cache device, loses no acknowledged transaction across 1000 simulated power cuts, on both architectures, on the weekly job in shards of at most 5.5 hours
- [ ] the in-guest suite passes with every registered counter narrower than 64 bits started one minute before its wrap (§25.7), on both architectures
- [ ] 72 hours of guest uptime on each architecture, in a 4-vCPU, 2 GiB guest under TCG running fork and exec, file I/O with `fsync`, and TCP to a peer on the runner, carried across shards by the §25.7 soak job, end with no panic, and §25.7's slope check projects under 1% growth over 30 days for frame, heap, slab, and descriptor counts and each vibefs volume's used blocks and used inodes; the same job runs one 5.5-hour shard of the workload under KVM on the KVM runner (4 vCPUs, 4 GiB) with the same checks; from this phase on the soak runs weekly on `main`, and a red soak blocks the next release like any scheduled job (F049)
- [ ] tag `phase-25` and cut the next release

### 25.1 Machine checks
- [ ] MCA on every CPU, where §10.6 already sets `CR4.MCE`: the bank count from `MCG_CAP`, every bank enabled, and any status found at boot logged as an error from the previous boot before it is cleared (F026)
- [ ] the `#MC` handler on its §2.1 IST stack reads `MCi_STATUS`, `MCi_ADDR`, and `MCi_MISC`, grades severity (corrected, UCNA, SRAO, SRAR, fatal) by Intel's encoding, and by AMD's `Deferred` and `Poison` bits on AMD CPUs, and takes no lock that normal code holds
- [ ] broadcast machine checks rendezvous every CPU and elect one to decide; local machine checks (`LMCE`) stay on one CPU; only fatal severity panics; QEMU's `mce -b` broadcasts on an Intel CPU model, and the CPU's `lmce=on` offers `LMCE`
- [ ] corrected errors found by a poll timer with a per-bank threshold, the path QEMU exercises, since it models no CMCI
- [ ] records into a per-CPU lock-free ring with a loom model (§10.8), drained to the log and to Linux's `/dev/mcelog` (`struct mce` records, `MCE_GET_RECORD_LEN`, `MCE_GET_LOG_LEN`, `MCE_GETCLEAR_FLAGS`), which unmodified `mcelog` reads, built statically in §13.11's digest-pinned Alpine container and pinned under §14.10, since Alpine does not package it; the decoder in the portable half, host-tested against recorded bank values
- [ ] every machine-check record carries the CPU's microcode revision from `IA32_BIOS_SIGN_ID`, which QEMU sets from the CPU's `ucode-rev` property, since errata fixed in microcode surface as machine checks

### 25.2 Platform errors
- [ ] SMBIOS from Limine's response, captured in `BootInfo` (§10.3) and parsed in the portable half with host tests: system identity, memory devices, and slot names, so an error names a DIMM rather than an address
- [ ] HEST, BERT, ERST, and EINJ parsed like the §2.4 tables in the portable half, host-tested against the server reports of the linuxhw/ACPI corpus (Phase 20), each table rebuilt from its raw bytes, and fuzzed; BERT records from the previous boot logged at boot
- [ ] GHES and GHESv2 error sources: the error status block read, its CPER records consumed, and GHESv2's read-ack register written, since QEMU refuses the next record until it is; the reader in the portable half, host-tested over simulated status blocks against the HEST entries of the linuxhw server reports, polled sources among them; on aarch64, GPIO-signal sources arrive as a Notify to the Hardware Error Device (PNP0C33) through the Generic Event Device's `_EVT` method (QEMU's `virt`), run on §20.2's interpreter
- [ ] an aarch64 harness variant with `-machine virt,ras=on` and no `acpi=off`, booting through §20.7's ACPI path, and a harness helper that builds a CPER memory-error record for an address the in-guest test names and injects it with `inject-ghes-v2-error`; QEMU returns success without injecting on a machine without ACPI, so the helper fails the run unless the guest logs the record
- [ ] CPER records decoded for memory, processor, and PCIe sections, host-tested against records QEMU's `scripts/ghes_inject.py` builds; a memory section's module handle names its DIMM through SMBIOS type 17
- [ ] PCIe Advanced Error Reporting: correctable errors counted per device; an uncorrectable error resets the link and calls an `error_detected` hook on the §6.1 `Driver` trait, so a failed NIC costs the NIC and not the machine
- [ ] aarch64: synchronous external aborts decoded from `ESR_EL1` into the §25.3 error kinds, with an in-guest test that reads a physical address no device or memory answers, which QEMU reports as a synchronous external abort; SError decoding and the RAS extension's error-record reader in the portable half, host-tested against recorded syndrome and record values

### 25.3 Memory error recovery
- [ ] a poisoned flag in §12.1's frame metadata; a poisoned frame never returns to the buddy allocator
- [ ] action-required errors in user memory, with Linux's semantics: the consuming thread gets `SIGBUS` with `BUS_MCEERR_AR` and the address; every other mapping, found through §12.1's reverse map, becomes a poisoned entry that raises the same signal on access, or gets `BUS_MCEERR_AO` at once when its process chose early kill through `PR_MCE_KILL`; a clean page-cache page is dropped and reread instead
- [ ] action-optional errors handled from a work item: the page unmapped the same way, and `BUS_MCEERR_AO` sent at once only to processes that chose early kill, so no process dies for a page it has not touched
- [ ] soft offlining: a frame past a corrected-error threshold has its contents migrated and is retired, tested with repeated `mce` injections at one address
- [ ] the retired-frame list kept across reboots in an EFI variable (§20.9) and applied before any user page is allocated
- [ ] an error in kernel memory panics with the physical address and the frame's owner from §12.1's accounting, rather than as an unexplained crash

### 25.4 kexec and crash capture
- [ ] Linux's `kexec_file_load`, with `KEXEC_FILE_ON_CRASH` for the capture kernel, and `reboot` with `LINUX_REBOOT_CMD_KEXEC` for the jump, so unmodified `kexec` from kexec-tools drives both: load a kernel and initrd, verify the kernel's signature against the §18.7 keys, and stage the jump
- [ ] measured kexec: when `kexec_file_load` accepts an image, after its signature check, the kernel extends PCR 10, where Linux's IMA measures a kexec image, with the SHA-256 of the kernel, the initrd, and the command line, and appends the three events to the event log it hands over in the serialized `BootInfo`; the new kernel's §18.7 check replays that log, so it attests the kernel that runs. No jump path touches the TPM, the crash path above all, since the panic path maps nothing and takes no lock (§22.2); an image loaded and never started stays in the log as a load event. §30.4's and §30.6's jumps inherit this. An in-guest test under swtpm kexecs to the same release's kernel, and its §18.7 check prints `boot: verified` with that kernel's digests in the log
- [ ] a direct entry that takes a serialized `BootInfo` (§10.3), so a kernel started by kexec needs neither Limine nor firmware, and `BootInfo` stays the only boot input it reads
- [ ] the serialized `BootInfo`, and §30.6's handover table beside it, are a versioned format documented in `docs/` before any code, because a kernel hands them to a different kernel version, older or newer (§30.4's update reboots, §30.6's live update): a magic, a format version, and length-prefixed tagged records, each tag marked required or optional; a kernel skips an optional tag it does not know and refuses a handover with an unknown required tag, with a named reason on serial, before it touches memory. Host tests parse records written by the previous release's code and a stream with an unknown optional and an unknown required tag, and an in-guest test kexecs from the release before to the current kernel and back
- [ ] `kexec_file_load` does for the new image what Limine does for a firmware boot: it draws the slide from §14.7's CSPRNG, applies the image's §18.2 relocations, builds the initial page tables the entry runs on (the image at the slid base, the HHDM at an offset it also draws), and writes both offsets and a fresh seed for §18.2's region bases into the serialized `BootInfo`, so §18.2's rule that the kernel never computes its own base holds; DESIGN §4.1 names the loader as a second source of the base
- [ ] `shutdown` on the §6.1 `Driver` trait, the same quiesce function as the stop step of `remove` and run in DESIGN §12.2's order, children and consumers first, quiescing DMA on every device before the jump, since a NIC still writing into the old kernel's buffers corrupts the new one
- [ ] after every driver's `shutdown`, the PCI core clears Bus Master Enable on every function, bound or not, as Linux does before a kexec, and the new kernel's drivers set it only after resetting their devices (§20.9); an in-guest test kexecs with a virtio-blk request in flight and a PCI function no driver binds, and the new kernel reads Bus Master Enable clear on every function before its first probe (F116)
- [ ] before a non-crash jump, every CPU but the one making it leaves the old kernel: on aarch64 it goes offline through §19.6 with PSCI `CPU_OFF`, so the new kernel's `CPU_ON` (§11.4) does not return `ALREADY_ON`; on x86_64 it leaves VMX or SVM operation (§21.1), since VMX root operation blocks INIT, and halts with interrupts off until the new kernel's INIT-SIPI
- [ ] x86_64 entry through a purgatory that checks the loaded image's digest; aarch64 entry with the MMU and caches off at the entry exception level, after cleaning to the point of coherency
- [ ] a capture region reserved at boot by a `crashkernel=` option on the §10.2 command line; with a capture kernel loaded, the panic path stops every other CPU with the §25.5 NMI IPI instead of the §4.9 halt broadcast, which a CPU with interrupts off never takes, then jumps into the capture kernel; on aarch64 the IPI is a GICv3 pseudo-NMI, and on GICv2 the §11.3 panic-halt SGI; DESIGN §2.5 records this path beside the Fixed `0xFE` halt
- [ ] `/proc/iomem` in Linux's format: the `System RAM` and `Reserved` ranges from `BootInfo`'s memory map, with the `crashkernel=` region nested under its RAM range as `Crash kernel`, since `kexec -p` from kexec-tools refuses to load a capture kernel without that range; the addresses read as zero to a reader without `CAP_SYS_ADMIN` (§18.6), as on Linux; the format host-tested against output captured from a Linux guest booted with `crashkernel=`, as §14.9's formats are
- [ ] each stopped CPU saves its interrupted registers as an ELF note, then leaves VMX or SVM operation on x86_64 and calls PSCI `CPU_OFF` on aarch64, as Linux's crash stop does; the panicking CPU saves its own; a CPU that does not answer within a bound is named in the notes; the capture kernel boots with Linux's `maxcpus=1` on the §10.2 command line, since such a CPU is still on, and the harness expects no `smp: ap online` line from it
- [ ] the capture kernel presents the old memory as an ELF core at `/proc/vmcore`, as Linux's does, and writes it filtered, reading §12.1's frame metadata from the dump to drop free, zero, page-cache, and user pages
- [ ] the vmcore written to a local disk, or over TCP to a host collector when there is none
- [ ] the KASLR offset (§18.2) and the build id in the vmcore notes; the §10.7 core tool reads the capture kernel's ELF vmcores as well as the harness's QEMU dumps

### 25.5 Watchdogs and lockups
- [ ] a soft lockup detector reports a CPU that stays on one thread longer than a threshold with interrupts on; a thread that never yields is preempted by the §3.3 timer tick, and a spin with interrupts off is the hard lockup detector's. A per-CPU watchdog thread above every §19.4 real-time priority records when it last ran, and a per-CPU timer that fires every 4 s, in §19.6's tickless idle too, reports the CPU's current thread and backtrace when that record is older than the threshold
- [ ] a hard lockup detector on NMIs: GICv3 pseudo-NMIs through priority masking on aarch64, raised by the PMU overflow interrupt, which changes how `InterruptGuard` masks there; on x86_64, whose hosted guests have no PMU (Phase 19), a buddy check: each CPU checks that the next online CPU's count of the soft lockup detector's 4 s timer has advanced since its own previous check, and sends that CPU the NMI IPI for its backtrace when it has not; that timer fires in §19.6's tickless idle too, so an idle CPU is not reported
- [ ] the NMI handler acts on a per-CPU request word (panic stop, backtrace, lockup check) and treats an NMI with no request pending as external: the CPU that takes it queues an all-CPU backtrace (the box below) and returns, printing nothing in NMI context (DESIGN §2.2), where today every NMI halts the kernel (`arch::idt`'s `nmi` handler)
- [ ] an all-CPU backtrace on demand through the NMI IPI, from the shell, from a serial break sequence, and from QMP `inject-nmi`, which works when the scheduler does not; each CPU writes its backtrace in NMI context only into a lock-free per-CPU buffer, and the CPU that asked prints the buffers outside NMI context (DESIGN §2.2); `inject-nmi` reaches the BSP through the LINT1 routing §20.1 programs from the MADT, and the harness sends it under TCG and under KVM on the KVM runner and finds a backtrace for every online CPU (F096)
- [ ] each NMI backtrace walk reads only the stacks §10.7's walker accepts, so the NMI handler takes no fault, since a fault there would also overwrite the CR2 of a `#PF` whose stub has not yet saved it (DESIGN §5.10 rule 9); a fault's `iretq` would unblock NMIs, and the next NMI would land on the same IST stack (F139)
- [ ] the §3.4 blocked-thread sweep covers every blocked thread, with a deadline or without one, and reports a thread blocked longer than a threshold with its stack and the lock or wait queue it is on, not only its id; an in-guest test blocks a thread with no deadline on a wait queue nothing wakes and finds the report naming that queue; today the sweep, which §10.7 made fire, sees only waiters with a deadline (F111)
- [ ] `/dev/watchdog` with Linux's ioctls (`WDIOC_KEEPALIVE`, `WDIOC_SETTIMEOUT`, `WDIOC_GETBOOTSTATUS`) over §20.6's watchdog drivers (`i6300esb`, the ICH9 TCO timer on `q35`, and the SBSA generic watchdog on `sbsa-ref`); init (§14.3) keeps feeding it after §22.2's health check commits, and stops when a critical service misses its own heartbeat
- [ ] §22.2's panic-reboot policy records the reason in the §25.6 persistent record and, when a capture kernel is loaded, takes the §25.4 vmcore before the reboot
- [ ] the reset reason (watchdog, panic, machine check, power) reported at boot from the persistent record or the watchdog's status

### 25.6 Persistent records and remote console
- [ ] an EFI-variable backend for the panic record over §20.9's runtime services and an ERST backend over §25.2's table, beside §20.1's reserved-RAM record, for machines where RAM does not survive a reset; under QEMU the ERST store is `-device acpi-erst` on a shared `memory-backend-file`, so it outlives the QEMU process
- [ ] the panic path takes §20.9's runtime-services lock only with a trylock; when that fails, or the panic arose inside a runtime call, it skips the EFI variable and writes the ERST or reserved-RAM record, since UEFI runtime services are not reentrant
- [ ] netconsole: log lines sent as UDP from a NIC driver in polled mode, usable from the panic path with interrupts off; virtio-net and e1000 first
- [ ] netconsole's panic path sends on a transmit queue reserved for it where the device has more than one; elsewhere it takes the transmit lock with a trylock and, when a stopped CPU holds it, resets the transmit ring before sending; an in-guest test panics while another CPU is inside the driver's transmit function, and the harness's listener receives the whole dump (F135)
- [ ] the harness gains a netconsole listener and asserts markers from it as it does from serial

### 25.7 Counter wraps, durability, and long runs
- [ ] every kernel counter narrower than 64 bits declared through one wrapping-counter type that records its expected rate; DESIGN's table of wrap times is generated from those declarations
- [ ] a `make check` script that fails on a 32-bit or 16-bit field named as a counter (`*_count`, `*_ticks`, `*_seq`, `*_gen`) outside that type
- [ ] a boot option on the §10.2 command line that starts every registered counter one minute before its wrap, run as a scheduled test variant
- [ ] vibefs v2 `fsync` fast enough for Phase 30's database line: if §14.8's measurement put v2's `fsync` p50 above twice ext4's, v2 gains a per-volume intent log as an incompat feature (VIBEFS.md §15), so an `fsync` writes that file's changes as one log record and flushes once, and mount replays the log after the newest superblock; §12.5's crash-state enumerator and §38.3's commit specification cover the log, and the SQLite line below runs with it. Otherwise this box closes on §14.8's record, with the numbers
- [ ] a test program over SQLite from the §14.9 mirror runs WAL-mode transactions against §12.5's volatile-cache device, cuts power at a random point in each iteration, and checks that every acknowledged transaction survived
- [ ] the soak job: a weekly workflow per architecture of consecutive shards under the Era VI long-run rule, each restoring the guest, its disk images, and its counter series from the previous shard's artifacts and saving them when its time is up; QEMU is the §10.1 pin, so every shard restores into the same version and machine type, and the guest has no device that blocks migration, such as §17.4's host mount
- [ ] a soak shard whose job ends without a harness verdict (GitHub reports the runner lost or the job cancelled, and the shard's artifacts hold no harness result) is rerun once from the artifacts it started from, with the rerun and its cause in the job summary; a second loss of the same shard is red; a shard in which the harness saw a guest timeout, a panic, a watchdog reset, or a failed check is red and never rerun (F021)
- [ ] the soak's TCP peer runs on the runner and restarts with each shard; a connection cut at a shard boundary is counted and reopened, not failed
- [ ] the soak workload and its counter sampling scripted in `tests/`, with a leak check that fits a slope to each counter rather than comparing two points, run at the end of every shard after the first over the series carried so far, so a leak fails the shard it shows in
- [ ] the soak workload keeps its live file set constant, and the sampled counters include each mounted vibefs volume's used blocks and used inodes, so an on-disk leak shows as a slope; vibefs v1 leaked blocks on every mount session that committed and on every write that failed in `add_extent` after allocating its block, and `fsck` reported no errors (F049, F051)

### 25.8 Stretch: BMC
- [ ] IPMI over KCS and SSIF against QEMU's simulated BMC (`ipmi-bmc-sim` with `isa-ipmi-kcs`, and `smbus-ipmi`) on x86_64: panics and machine checks written to the BMC's system event log, the BMC watchdog as a second watchdog, and Linux's `/dev/ipmi0` interface, so unmodified `ipmitool` from the §14.9 mirror reads the log

---

## Phase 26: Cloud-ready Images

**Goal.** vibeOS images that boot on the virtual machine monitors and devices public clouds present, and
configure themselves from each platform's metadata with nobody at a console, proved on free VMMs and
metadata mocks.

**Unlocks.** Images ready for the clouds' VMMs, metadata services, and virtual devices, which the cloud
entries in [Funded goals](#funded-goals) take to the clouds themselves with the drivers no free emulator
covers (AWS's ENA, Google's gVNIC). MicroVM hosts (Firecracker, cloud-hypervisor, QEMU's `microvm`) as a
place vibeOS runs.

**Architectures.** Both. aarch64 cloud VMs boot through UEFI and describe themselves with ACPI rather
than a device tree, so this phase needs §20.7's ACPI path. Firecracker and cloud-hypervisor need KVM,
OpenVMM needs KVM on a Linux host, and QEMU's `microvm` is x86 only, so their gate lines are x86_64 on
the KVM runner: GitHub's arm64 runners have no KVM, and of the three only OpenVMM runs on macOS, under
Hypervisor.framework, where §26.7 records aarch64 VMBus. The aarch64 lines run as QEMU `virt` guests
booted through edk2 with ACPI, the shape aarch64 cloud VMs have, under TCG. The metadata emulator and
mocks run on the runner beside the VMM, so every provisioning path runs on both architectures. No free,
licensed emulator of AWS's ENA or Google's gVNIC exists, so those drivers, the clouds themselves, and
aarch64 microVMs, which need an aarch64 KVM host, are [Funded goals](#funded-goals).

**Exit gate**
- [ ] the §26.2 service configures a fresh image from each platform's metadata, served by the §26.1 emulator through QEMU's user network: AWS through IMDSv2, with any request that lacks a session token refused, Google, Azure IMDS with the ready report to the WireServer, and cloud-init's NoCloud from a `CIDATA` disk and from `ds=nocloud` in the SMBIOS system serial; each run sets the hostname, installs the authorized key that an SSH login from the runner then uses, and runs the user data once per instance id, again when the id changes, and not on a reboot with the same id; on both architectures under TCG (2 vCPUs, 1 GiB), on the nightly job
- [ ] the AWS and Google paths also pass against pinned `amazon-ec2-metadata-mock`, with IMDSv2 required, and the community `gce_metadata_server`, so the emulator's shapes are checked against implementations vibeOS did not write
- [ ] under KVM on the KVM runner (2 vCPUs, 512 MiB), the kernel ELF boots through its §26.4 PVH entry, with no firmware and no Limine, to `shell ready` on Firecracker, over virtio-mmio and again over PCI with `--enable-pci`, on cloud-hypervisor, and on QEMU's `microvm` with `acpi=on,pcie=on`, with root on virtio-blk and network on virtio-net on each; on Firecracker the AWS path provisions the guest from MMDS V2, and an SSH login uses its key
- [ ] cloud-hypervisor also boots the release's raw disk image through edk2's `CLOUDHV.fd` and Limine, and provisions it through NoCloud, under KVM on the KVM runner
- [ ] under OpenVMM (§26.6) on the KVM runner, an x86_64 image boots through OpenVMM's UEFI firmware (`mu_msvm`) with root on storvsc over VMBus and network on netvsc, and in a second run on OpenVMM's emulated MANA; it reports ready to the §26.1 WireServer emulator and accepts an SSH login with the key from the emulator's IMDS
- [ ] on first boot under every VMM above, and under QEMU on both architectures, the image grows its root partition and vibefs v2 filesystem to a disk enlarged since the image was built, and generates per-instance SSH host keys whose fingerprints it prints on the console
- [ ] a volume attached to a running guest appears under a persistent name and detaches cleanly, in two clouds' shapes, under QEMU on both architectures (TCG, 2 vCPUs, 1 GiB): an NVMe controller hot-added on a PCIe root port, whose serial is the volume id and whose model is `Amazon Elastic Block Store` (QEMU 11.1 or later), named by that id as EBS volumes are; and a namespace attached to a running controller with the Namespace Attachment command and announced by the namespace-attribute-changed event, as Google's persistent disks are
- [ ] a deliberate panic under QEMU, Firecracker, cloud-hypervisor, and OpenVMM prints its text on the serial console the harness captures and resets the guest under §22.2's panic policy
- [ ] the nightly job boots the images §26.6 builds from `main` through every VMM backend and provisioning path above, asserts the DESIGN §8.3 markers from each VMM's console, runs the SSH smoke test, and fails when any guest does not come up; the fixed VHD boots under QEMU from the file as published (`format=vpc`)
- [ ] tag `phase-26` and cut the next release

### 26.1 Platform and metadata
- [ ] the platform identified from §25.2's SMBIOS fields (vendor, product, chassis asset tag), not by probing endpoints until one answers; the harness sets each cloud's values with QEMU's `-smbios`, and where a VMM gives no SMBIOS or not a cloud's values (Firecracker, OpenVMM), `ds=` on the §10.2 command line names the source, as cloud-init reads it
- [ ] one metadata client with a backend per platform: AWS IMDSv2 with a session token from `PUT` and the hop limit respected, Google with `Metadata-Flavor: Google`, and Azure IMDS with `Metadata: true`
- [ ] a metadata emulator in `tests/harness`, standard library only, serving every platform's shape on `169.254.169.254` (and Azure's WireServer on `168.63.129.16`) through a QEMU user netdev whose `net=` covers both addresses (such as `168.0.0.0/7`, since `guestfwd` accepts only addresses inside it), with a `guestfwd` to the emulator for each endpoint and `restrict=on`, so no metadata request reaches a real endpoint: a GitHub runner is itself an Azure VM, with Azure's IMDS and WireServer at those addresses
- [ ] the VMMs other than QEMU run in a network namespace of their own on the runner, in which the metadata addresses are local addresses the emulator listens on and no route leaves the runner
- [ ] instance credentials from metadata never logged, and on AWS fetched only through the token path
- [ ] TSC frequency from CPUID leaves `0x15` and `0x16`, or from the hypervisor (Hyper-V's frequency MSR, §21.4's KVM clock), before any HPET or PIT calibration, since Firecracker's and `microvm`'s guests have no HPET; the `time: calibrated <source>` line names the source
- [ ] the console on each platform: COM1 on x86_64, which each VMM here provides, and the SPCR-described UART on aarch64 (§20.7)

### 26.2 Provisioning
- [ ] a `cloud-init`-shaped service under §14.3's init, run once per instance id: users and authorized keys, hostname, network from metadata with DHCP as the default, and user data as a script or a declarative file of users, packages, files, and commands
- [ ] cloud-init's NoCloud datasource: a vfat or ISO 9660 seed labelled `CIDATA`, or `ds=nocloud;s=<url>` from the §10.2 command line or the SMBIOS system serial, with the seed's `meta-data`, whose `instance-id` it requires, `user-data`, `vendor-data`, and `network-config`
- [ ] the root partition grown in place (GPT backup header moved, partition extended), and its vibefs v2 filesystem grown by the initrd before root is mounted, through a grow in the shared format code that host tests cover
- [ ] per-instance SSH host keys, with their fingerprints printed on the console so a first login can be checked
- [ ] Azure: provisioning data from IMDS, and a ready report to the WireServer, which a deployment waits on; the UDF provisioning disc is not read
- [ ] a status command listing which stages ran and their output, also in the system log

### 26.3 Volumes
- [ ] volume attach and detach two ways: through §20.9's hotplug where each volume is its own NVMe controller (EBS on AWS), and as a namespace attached to or detached from a running NVMe controller, announced by the namespace-attribute-changed asynchronous event and read from the changed-namespace log (Google Cloud's persistent disks); under QEMU the in-guest test attaches a namespace created with `detached=on` by sending the Namespace Attachment command through a second controller of the same `nvme-subsys`, as a cloud's control plane would
- [ ] the cloud's volume id added to §20.9's persistent names where the device reports it: EBS puts it in the NVMe serial number, with `Amazon Elastic Block Store` as the model; QEMU sets both (`serial=`, and `model=` from 11.1) but not EBS's PCI vendor ID or the device name it keeps in the vendor-specific Identify bytes, so the name is decided from the model and serial alone

### 26.4 MicroVMs
- [ ] a PVH entry, the `XEN_ELFNOTE_PHYS32_ENTRY` note in the kernel ELF, that builds `BootInfo` (§10.3) from the PVH start info's memory map, command line, and modules, as §25.4's kexec entry builds it from a serialized one, so Firecracker, cloud-hypervisor, and QEMU's `microvm` start the kernel directly with no firmware and no Limine; with no firmware, TPM, or Secure Boot, the §18.7 check prints `boot: unverified`, and the microVM relies on its VMM, which DESIGN §2.10 trusts for everything
- [ ] the PVH entry, which the VMM starts in 32-bit protected mode with paging off, does what Limine does for a firmware boot, in 32-bit code in a section of its own: it applies the image's §18.2 relocations for a slide drawn from `RDSEED` or `RDRAND` (zero, and logged, on a CPU with neither), enters long mode on page tables that map the image at the slid base and the HHDM at a drawn offset, and passes both offsets and a seed for §18.2's region bases in `BootInfo`; DESIGN §4.1 names it, beside §25.4's `kexec_file_load`, as a place a base is computed outside Limine
- [ ] virtio over MMIO on x86_64 through §11.5's virtio-mmio transport: devices found from the ACPI tables Firecracker and `microvm` generate, or from Linux's `virtio_mmio.device=` option on the command line where a VMM gives no ACPI
- [ ] the virtio drivers meet the virtio 1.2 driver requirements that QEMU's devices do not exercise, since Firecracker's and cloud-hypervisor's devices are the first non-QEMU devices they meet: on PCI, each `queue_msix_vector` is read back after it is written and `NO_VECTOR` fails the probe, and an MSI-X interrupt reads no ISR; on both transports, a multi-dword configuration field such as `capacity` is read inside a `config_generation` loop, and `F_SIZE_MAX` and `F_SEG_MAX` are negotiated only when requests honor `size_max` and `seg_max`; host tests against the §6.5 simulated device refuse a vector and change the generation mid-read (F122)
- [ ] reboot and the §22.2 panic reset through the i8042 reset line where a VMM offers no other (Firecracker)
- [ ] Firecracker's MMDS V2 served to the §26.1 AWS backend with its session token, as IMDSv2 is
- [ ] the time from VMM start to `shell ready` recorded per release for each microVM, in the gate's guest shape under KVM on the KVM runner

### 26.5 Hyper-V: VMBus and MANA
OpenVMM, Microsoft's MIT-licensed VMM, emulates the devices an Azure VM has. QEMU's Hyper-V
enlightenments under KVM (the `hv-*` CPU flags) are a second host for the parts below VMBus.

- [ ] the Hyper-V hypercall interface and the synthetic interrupt controller through their x86_64 MSRs, tested under QEMU with `hv-vpindex`, `hv-synic`, `hv-time`, and `hv-stimer` on the KVM runner, and under OpenVMM
- [ ] VMBus as a bus in the §6.1 device model: channel offers, GPADL buffer sharing, and ring buffers with the host's interrupt-suppression protocol, host-tested against a simulated host as §6.5's rings were
- [ ] storvsc, with a small SCSI command layer (`INQUIRY`, `REPORT LUNS`, `READ CAPACITY(16)`, `READ(16)`, `WRITE(16)`, `SYNCHRONIZE CACHE`, `UNMAP`) that virtio-scsi can reuse
- [ ] netvsc: NVSP and RNDIS messages, send and receive buffers, and one channel per CPU
- [ ] MANA, Azure's current NIC, over OpenVMM's emulated device (`--mana`): its GDMA queues and a queue pair per CPU
- [ ] the Hyper-V reference TSC page as the clock; §21.4's Hyper-V detection finds it, and this uses it, tested under QEMU with `hv-time` on the KVM runner

### 26.6 Images and CI
- [ ] the §22.1 release emits a raw disk image, a Google `disk.raw` tarball, a fixed-size VHD aligned to 1 MiB, and the kernel ELF with its PVH note for direct boot, byte-reproducible like its other artifacts
- [ ] the harness gains VMM backends for Firecracker, cloud-hypervisor, and OpenVMM beside QEMU, each pinned by release and hash, OpenVMM built from its source release since it ships no binaries, and each asserting the same marker contract from its serial console
- [ ] `amazon-ec2-metadata-mock` and `gce_metadata_server` pinned by version and hash as test inputs that are never shipped, and one script that sets up the emulator, the mocks, and each VMM's network namespace for the nightly job

### 26.7 Stretch: more platforms
- [ ] aarch64 VMBus: the Hyper-V hypercall interface and SynIC through hypercalls, with storvsc and netvsc, under OpenVMM on the dev host's Hypervisor.framework, as a §10.9 record
- [ ] virtio-scsi on the §26.5 SCSI layer, for older Google machine types and KVM clouds that use it, under QEMU
- [ ] OpenStack's metadata service and `config-2` config drive in the §26.1 emulator and the §26.2 service
- [ ] Xen HVM guest support under QEMU's KVM Xen emulation (`xen-version=` on `-accel kvm`) on the KVM runner: the Xen hypercall page, event channels, and the Xen PV clock, the shape of EC2's Xen instance types

---

## Phase 27: Scale-up

**Goal.** One kernel that runs correctly on a thousand CPUs and a terabyte of memory, with no table
sized by a constant, no interrupt route that stops at 255 CPUs, and no boot work that grows with memory
the kernel does not use.

**Unlocks.** A kernel ready for machines with hundreds of CPUs and terabytes of memory, including the
large cloud shapes; the rented bare-metal hosts in [Funded goals](#funded-goals) measure it on real
cores. A Phase 21 host that holds many guests. The CXL entry in [Beyond](#beyond).

**Architectures.** Both. Past 255 CPUs, x86_64 needs 32-bit APIC IDs in every interrupt route: §20.1's
x2APIC mode and interrupt remapping, or KVM's extended destination ID in a guest without an IOMMU.
aarch64 needs GICv3, since GICv2 stops at 8 CPUs. The gates run under QEMU on 4-vCPU runners, so they
check correctness and counts, not speed. TCG takes x86_64 to 4096 vCPUs on `q35` from QEMU 9.0, the
first release whose TCG models x2APIC, and OVMF hands off in x2APIC mode above 255; it takes aarch64 to
512 on `virt` with GICv3. KVM's extended destination ID exists only under KVM, so it is checked on the
KVM runner with QEMU's split irqchip. Guest memory past the runner's 16 GB is sparse: a
`memory-backend-ram` with `reserve=off`, since Linux's default overcommit refuses one mapping larger than
RAM and swap, and on x86_64 under TCG `phys-bits=48`, since the default of 40 bits stops below 1 TiB; the
host commits only the pages the guest touches. Speedups on real cores are a
[Funded goal](#funded-goals).

**Exit gate**
- [ ] under TCG on the weekly job, 1024 vCPUs on x86_64 `q35` (`-cpu max`, OVMF, `intel-iommu,intremap=on,eim=on`, 8 GiB) and 512 on aarch64 `virt,gic-version=3` (8 GiB) reach `smp: done` and `shell ready`; every CPU answers a §4.9 call-function IPI, and an MSI-X vector of a virtio-net device steered to the highest-numbered CPU is delivered there
- [ ] under KVM on the KVM runner, a 288-vCPU, 4 GiB x86_64 guest with no IOMMU, `kernel-irqchip=split`, and `kvm-msi-ext-dest-id` on the `-cpu` line delivers an MSI-X vector to a CPU whose APIC ID is above 255 through KVM's extended destination ID (§27.1)
- [ ] in those TCG guests, the time from the first SIPI or `CPU_ON` to `smp: done`, printed on the boot line, is less than 8 times the time a guest with a quarter as many vCPUs takes in the same job, so bring-up is not quadratic in the CPU count, on both architectures
- [ ] a 1 TiB guest with sparse memory boots to `shell ready` under TCG with 8 vCPUs in no more than 1.25 times the time of a 4 GiB guest with the same vCPUs in the same job, `meminfo` reports the full total, and the QEMU process's resident memory at `shell ready` is under 4 GiB, so the kernel touches no memory it does not use; x86_64 on `q35` and aarch64 on `virt`
- [ ] no kernel table is sized by a CPU, memory, or device-count constant: the 64 MiB heap region is gone (§12.6), as the 8 GiB physmap cap went in §11.2 and the CPU caps, the PCI scan cap, and the BAR mapping cap in §20.1, and `scripts/check_limits.py` in `make check` fails on a `MAX_*` array or table bound outside the §10.4 `limits` module, except the specification-fixed bounds it lists with their source (such as `MAX_BARS`, six per PCI function)
- [ ] counted, not timed, in the 1024-vCPU and 512-vCPU guests under TCG: an `munmap` in a process whose threads have run on 4 CPUs sends TLB shootdown IPIs to at most those 4 on x86_64 and none on aarch64; a grace period completes with no CPU polling more than 64 others (§27.5's RCU tree); and 512 threads contending on one `SpinMutex` each acquire it at least once within the harness profile's scaled timeout (§27.5 checks queue order at `-smp 4`)
- [ ] QEMU's `edu` device with a 32-bit DMA mask works in a 16 GiB guest with sparse host backing: through the §27.2 bounce pool with the IOMMU off, and through IOVAs below 4 GiB with the §18.1 IOMMU on; under TCG with 2 vCPUs, on both architectures
- [ ] in an 8 GiB guest under TCG with 4 vCPUs (`-cpu max` on x86_64), a 4 GiB anonymous mapping touched sequentially is backed by 2 MiB pages, counted per process; a 1 GiB `MAP_HUGETLB` mapping succeeds from the boot pool; after 10 million cycles of the §27.4 fragmentation workload, compaction still satisfies an order-9 allocation; on both architectures
- [ ] tag `phase-27` and cut the next release

### 27.1 Interrupts past 255 CPUs
- [ ] x2APIC cluster-mode logical destinations, so an IPI to a set of CPUs is one ICR write per cluster
- [ ] KVM's extended destination ID, found in KVM's CPUID feature leaf (`0x40000001`) without waiting for §21.4's detection, so a guest with no exposed IOMMU routes MSIs to APIC IDs above 255; QEMU offers it with `kernel-irqchip=split`, and elsewhere §20.1's interrupt remapping does it
- [ ] x86_64 vectors allocated per CPU rather than from one global pool, since one MSI-X vector per queue per CPU exhausts a single space of about 200
- [ ] aarch64: GICv3 affinity routing over all four affinity levels, redistributors discovered for every core, one ITS collection per CPU, and SGIs sent with the range selector; on GICv2 the boot log says the machine is capped at 8 CPUs

### 27.2 DMA limits
- [ ] a DMA address mask per device, and a bounce pool (swiotlb-shaped) for devices that cannot reach every frame
- [ ] a zone below 4 GiB for devices with 32-bit DMA masks, which the bounce pool draws from
- [ ] IOVAs allocated inside the device's mask when a §18.1 IOMMU domain translates for it, so no bounce is needed

### 27.3 Large memory
- [ ] the KVA region in DESIGN §4.1 sized at boot from installed memory, instead of fixed at 64 GiB; §12.6 already sized the heap region and §20.1 the `ioremap` window
- [ ] frame metadata (§12.1) allocated per memory section so holes cost nothing, placed on its own node (§19.7), and initialized a section at a time when the allocator first reaches it, so boot touches only the metadata it uses
- [ ] a memory section joins its node's buddy allocator when the allocator first reaches it, with its metadata, and `meminfo` counts the sections not yet joined as free, so boot writes no free-list link into memory it has not used; today `insert_region` writes a 16-byte link into the first frame of every free 4 MiB block at boot, and since §11.2 the buddy takes all RAM, so in a 1 TiB guest those links touch 262144 pages, 1 GiB
- [ ] per-CPU free-frame lists in front of each node's buddy, since the buddy lock is the first thing many CPUs contend; §19.9 did the same for objects
- [ ] memory layouts with holes, ranges above 1 TiB, and eight NUMA nodes tested under QEMU `-numa`

### 27.4 Huge pages
- [ ] 2 MiB and 1 GiB pages for user memory, each one §12.1 unit of order 9 or 18: a 1 GiB pool reserved at boot, `MAP_HUGETLB` with `MAP_HUGE_2MB` and `MAP_HUGE_1GB`, and Linux's `hugetlbfs`
- [ ] transparent 2 MiB pages for aligned anonymous regions at fault time, and a background collapser for regions first faulted in 4 KiB pages
- [ ] compaction: movable pages migrated through §12.1's reverse map to rebuild free 2 MiB blocks, on demand and in the background, each old unit released through the gather once the invalidation of its PTEs completes (DESIGN §2.4)
- [ ] a fragmentation workload in `tests/`: seeded, mixed-order allocations and frees with long-lived unmovable ones among them, which the gate runs
- [ ] huge pages split on partial `munmap`, `mprotect`, and COW under DESIGN §4.6's split rule (unmapped everywhere through the reverse map, the unit's count frozen at the cache's reference plus the caller's, or the unit remapped and the split refused), with an in-guest test for each and one that holds a pin on the unit and finds the split refused and the 4 KiB fallback taken
- [ ] per-process and system counts of huge mappings, which the gate reads

### 27.5 Many CPUs
- [ ] a host test parses a MADT with 4096 processors, QEMU `q35`'s limit (types 9 and 10), through §20.1's tables; aarch64's CPU tables, from the device tree and from the MADT's GICC entries, are sized from the firmware CPU count the same way; an MPIDR listed twice yields one CPU and one logged warning, as a repeated APIC ID does in §20.1's `acpi::parse_madt` (F035)
- [ ] the thread table and every structure §10.4 sizes from the `limits` thread count, the wake inbox and the KVA free-list pool among them, sized at boot for the Phase 10 value (1024) plus each CPU's §4.8 idle thread and other per-CPU kernel threads, counted from the firmware CPU count, so a 1024-vCPU guest keeps the 1024 threads Phase 12 was tested against; once §23.4 has landed, its `kernel/threads-max` governs instead; a host test computes the limits for 4096 CPUs
- [ ] AP bring-up in parallel: batched INIT-SIPI on x86_64 and concurrent PSCI `CPU_ON` on aarch64, with the timer calibrated once and shared when the counter is invariant, instead of the §4.5 one-at-a-time sequence with its 10 ms per AP; the ready deadline scales with the number of APs started at once, since 1023 APs under TCG share the runner's 4 vCPUs, and an AP that misses it takes §20.1's INIT-and-leak path (F032)
- [ ] per-CPU areas, stacks, and run queues allocated on the CPU's own node
- [ ] `SpinMutex` becomes a queued (MCS) spinlock for every lock in one change, replacing the compare-and-swap lock (§3.5), since a test-and-set lock collapses under hundreds of waiters and grants the lock in no order (DESIGN §2.3, AGENTS.md rule 10): an uncontended acquire is one compare-and-swap on the lock word, and a waiter queues on its CPU's one node, spins on it with IF=0 servicing incoming work, and gets the lock in queue order, with a debug assertion that the node is free on entry. Where §21.4's paravirtual kick has landed, it moves onto the queue in the same commit (a queued waiter that has spun a bound halts, and whoever hands it the lock or the queue's head kicks it, as Linux's paravirtual queued spinlock does), its wait slots go, and §21.4's in-guest test runs again. A loom model beside §10.8's, whose variant lets a later waiter take the lock ahead of the queue's head, must reject it, and a §11.7 litmus test covers the hand-off
- [ ] in-guest at `-smp 4` under TCG on both architectures, four threads, one pinned to each CPU, each take one `SpinMutex` 10,000 times, and a `kernel_tests` counter finds every acquisition granted in the order its waiter queued
- [ ] where no paravirtual kick exists (TCG, HVF, aarch64 under KVM, x86_64 KVM without `PV_UNHALT`), a waiter whose vCPU is descheduled when the lock reaches it stalls every waiter behind it. The cost is measured, not assumed: the §19.3 lock-acquire microbenchmark on the §10.1 KVM leg in a 4-vCPU guest whose vCPU threads are pinned to 2 host CPUs, with no kick (`nopvspin` once §21.4 has landed), and the time from the first SIPI or `CPU_ON` to `smp: done` in the exit gate's 1024-vCPU and 512-vCPU guests, each before and after the first box above, are recorded in DESIGN §8.6. If the queued lock is worse by more than a quarter on either, `SpinMutex` takes a test-and-set path under a hypervisor that offers no kick, as Linux's x86_64 guests do, and DESIGN §2.3 records why
- [ ] lazy TLB for kernel threads (DESIGN §7.9), with the IPIs of each round counted per CPU: a kernel thread keeps the previous address space's root loaded; its CPU stays in the set, marked lazy, and holds a core reference to the address space (DESIGN §2.11) until it loads another root, released through the deferred put; a round that changes only leaf PTEs skips lazy CPUs, which catch up through §18.3's flush generation on their next switch to a user address space, the same one included; a round that frees page-table pages also sends lazy CPUs an IPI on x86_64; aarch64 keeps broadcast `tlbi ...is`, sends no IPI, and relies on the core reference to keep the root and its ASID. In-guest at `-smp 4` on x86_64: an `munmap` that frees a page-table page while a CPU in the set runs a lazy kernel thread sends that CPU an IPI. The cost of a switch to a kernel thread and the IPIs per round are recorded in the 1024-vCPU guest with lazy TLB on and off; if the numbers show no saving, lazy TLB is not kept: DESIGN §7.9's lazy paragraph goes, and kernel threads keep loading the kernel root (the empty TTBR0 on aarch64)
- [ ] load balancing hierarchical over SMT, core, cache, and NUMA levels, extending §19.4's topology and §19.7's nodes to 1024 CPUs and 8 nodes, with the balancing work per tick counted
- [ ] RCU (§19.5) grace periods tracked in a tree, so one CPU does not poll hundreds of others, with the CPUs each one polls counted
- [ ] harness profiles with scaled timeouts on the weekly job: `VIBEOS_SMP=1024` on x86_64 and `512` on aarch64 under TCG, and 288 under KVM on the KVM runner

### 27.6 Stretch: larger address spaces and hot-add
- [ ] 5-level paging (LA57) on x86_64 and 52-bit addresses (LPA2) on aarch64, for machines past 64 TiB, under TCG
- [ ] memory hot-add through ACPI or virtio-mem under QEMU, so a VM can grow without a reboot

---

## Phase 28: Network at Scale

**Goal.** Spread a NIC's traffic across queues and CPUs, hold a hundred thousand connections, and give
a guest a virtual function of its own, with the cost per packet and per connection measured against
Linux in the same guest on the same runner.

**Unlocks.** Network-bound services that scale with cores. Guests on SR-IOV virtual functions. The
RDMA entry in [Beyond](#beyond). A physical 100GbE NIC at line rate is a
[Funded goal](#funded-goals).

**Architectures.** Both. NIC drivers are PCI and shared. The throughput, packet-rate, and
connection-rate gates run under KVM on the KVM runner, with virtio-net on a multiqueue tap and the
runner's `vhost-net`, and the peer and load generator as processes on the runner; each number is
compared with Linux, the §23.6 reference kernel booted in the same guest shape with the same devices, in
the same job. GitHub's arm64 runners have no KVM, and macOS has no tap device and no `vhost-net`,
so the aarch64 lines are functional and run under TCG. The SR-IOV lines use QEMU's `igb`, a model of the
Intel 82576 with 8 virtual functions that QEMU documents as a way to test SR-IOV without hardware,
behind `intel-iommu` on x86_64 `q35` and SMMUv3 on aarch64 `virt`. Assigning a function to a §21.2 guest
runs vibeOS as a hypervisor inside the runner's guest, under the Era VI nested-virtualization rule.

**Exit gate**
- [ ] TCP between a 4-vCPU, 4 GiB guest and a peer on the runner, over virtio-net with 4 queue pairs on a multiqueue tap with `vhost-net`, 8 streams in each direction, reaches at least 80% of Linux's rate in the same guest shape and job, under KVM on the KVM runner
- [ ] the 64-byte UDP receive rate over the same device is at least 50% of Linux's in the same guest shape and job, under KVM on the KVM runner
- [ ] RSS places flows where the configured key and indirection table say: of 64 UDP flows of 10,000 packets each, whose source ports are chosen so the §28.1 Toeplitz hash puts 64/N on each of N receive queues, every flow arrives on its predicted queue, and each queue's interrupts land on its own CPU, shown by per-queue counters; on virtio-net with 4 queue pairs and QEMU's `rss=on` (vhost off, so QEMU computes the hash), and on `igb` with 4 queues set by `ethtool -L`, whose model hashes with the key and table the driver writes; in a 4-vCPU, 4 GiB guest under TCG, on both architectures
- [ ] an `epoll` server holds 100,000 idle TCP connections from a load generator on the runner, with kernel memory per idle connection under 10 KiB, in a 4-vCPU, 4 GiB guest under TCG on both architectures; under KVM on the KVM runner it also accepts new connections at no less than 70% of Linux's rate in the same guest shape and job
- [ ] an `igb` virtual function, enabled by vibeOS's physical-function driver behind the §18.1 IOMMU, is assigned through §28.4's VFIO to a 2-vCPU, 1 GiB vibeOS §21.2 guest, whose §28.3 VF driver carries TCP to a peer on the runner with every byte checked; the function is reset before a second guest gets it, and that guest finds none of the first one's state in it; in Phase 21's nested job on x86_64, and in its EL2 job and HVF record on aarch64
- [ ] tag `phase-28` and cut the next release

### 28.1 Multiqueue
- [ ] per-queue receive and transmit rings, one MSI-X vector each (from §27.1's per-CPU pools on x86_64), placed through the §6.3 affinity API with one queue per CPU up to the device's limit
- [ ] RSS: a Toeplitz hash, a programmable indirection table, and a configurable key, set by `ethtool` from the §14.9 mirror through Linux's `SIOCETHTOOL` ioctl
- [ ] the channel count set by `ethtool -L` through `ETHTOOL_SCHANNELS` and read through `ETHTOOL_GCHANNELS`; virtio-net's `VIRTIO_NET_F_RSS`, with the key, indirection table, and hash types sent over the control queue so the device picks the receive queue; the Toeplitz hash in the portable half, host-tested against the verification examples in Microsoft's RSS specification, which the placement gate picks its flows with
- [ ] software receive steering for devices without RSS, and transmit queue selection by the sending CPU
- [ ] per-queue statistics (packets, bytes, drops, interrupts, and polls), which `ethtool -S` reads

### 28.2 Offloads and batching
- [ ] TCP segmentation offload on `igb`, whose model segments in the device as the NIC does, and GSO, its software fallback at the driver boundary for devices without it; §15.1 already uses virtio-net's
- [ ] receive coalescing (GRO) in software, and virtio-net's large receive packets (`VIRTIO_NET_F_GUEST_TSO4` and `VIRTIO_NET_F_GUEST_TSO6`) accepted and passed up whole
- [ ] jumbo frames up to 9000 bytes end to end, with virtio-net's MTU taken from the device (`VIRTIO_NET_F_MTU`, QEMU's `host_mtu`)
- [ ] byte queue limits on transmit, so a deep ring does not add milliseconds of latency
- [ ] an interrupt followed by budgeted polling until the ring is empty, with adaptive interrupt moderation beside §19.6's coalescing
- [ ] socket busy polling for latency-bound services, measured against the interrupt path under KVM on the KVM runner

### 28.3 igb and its virtual functions
QEMU's docs warn that the `igb` model lacks many hardware features; a behavior the driver needs and the
model lacks is listed in `docs/` with the Linux driver's handling of it, not faked.

- [ ] the igb driver (§20.6) with multiple queues, RSS, and TSO, and the physical-function side of SR-IOV: the PF-VF mailbox, per-VF MAC and VLAN filters, and VF reset
- [ ] the `igbvf` virtual function driver, which §28.4 assigns: the mailbox to the PF, its queues, and function-level reset
- [ ] link state and speed reported; §25.2's `error_detected` hook implemented on both functions, tested with `pcie_aer_inject_error`
- [ ] one script that takes the vibeOS and Linux measurements back to back in one job on the same runner, with the same guest shape, devices, and peer

### 28.4 SR-IOV and passthrough
- [ ] the SR-IOV capability: virtual functions enabled, their BARs sized, and bound like any PCI device
- [ ] per-VF MAC and VLAN filters set from the physical function
- [ ] device assignment to a §21.2 guest through Linux's VFIO interface, so QEMU's `vfio-pci` uses it with `host=<bdf>`: `/dev/vfio/vfio` and a `/dev/vfio/<n>` node per §18.1 isolation group; an `iommu_group` link in each assignable device's §23.3 `/sys/bus/pci/devices/<bdf>/` directory pointing to `/sys/kernel/iommu_groups/<n>/`, whose `devices/` lists the group; binding to VFIO through the device's `driver_override` and `/sys/bus/pci/drivers/vfio-pci/bind`, as on Linux; the group mapped to guest memory, and its MSI-X delivered to the guest through §21.2's `irqfd`
- [ ] a DMA the assigned function is told to make outside its guest's memory is blocked by the §18.1 IOMMU and logged with the requester and address, with an in-guest test in the assigned guest that programs such a descriptor
- [ ] the assigned device reset between guests, so no state from one guest reaches the next

### 28.5 Connection scale
- [ ] TCP and UDP lookup tables sized at boot from memory, with Linux's `thash_entries=` and `uhash_entries=` overrides on the §10.2 command line, and never resized, as Linux's are, so no rehash runs under a lock or races an RCU lookup; listening sockets spread across CPUs with `SO_REUSEPORT` (§15.7) and per-CPU accept queues
- [ ] each socket lookup table that §19.5 left behind its lock measured again in the Phase 28 gate's accept-rate run, and DESIGN §7.7 records whether it moves to RCU (DESIGN §2.12), with the numbers that decided it
- [ ] TCP timers (retransmit, delayed ACK, keepalive, `TIME_WAIT`) on §19.4's per-CPU timer wheels, so 100,000 connections do not share one sorted list
- [ ] system-wide pressure thresholds below §15.1's global socket-memory limit, so a connection flood shrinks socket buffers before the OOM path runs
- [ ] kernel memory per connection measured and recorded in `docs/`
- [ ] the connection-scale load generator on the runner spreads its connections over enough source addresses on the tap that none needs more than the 28,232 ports of Linux's default ephemeral range, and in the accept-rate run closes each connection with a reset (`SO_LINGER` 0), so no runner port waits in `TIME_WAIT`

### 28.6 Stretch: steering and timestamps
- [ ] receive flow steering that follows the consuming thread's CPU
- [ ] hardware timestamping and a PTP hardware clock on `igb`, whose model timestamps PTP packets through its `TSYNCRXCTL` and `TSYNCTXCTL` registers, reached through Linux's `SO_TIMESTAMPING` and a `/dev/ptp<N>` clock
- [ ] virtio-net virtual functions through QEMU's composable SR-IOV (`sriov-pf=`, QEMU 10.1 or later), assigned as `igb`'s are

---

## Phase 29: Storage at Scale

**Goal.** Several disks act as one volume that grows, survives the loss of a disk, repairs silent
corruption, and reports a failing disk before it fails; an NVMe namespace is reached over more than one
path and written zone by zone; and an NVMe device runs as fast as Linux drives it in the same guest.

**Unlocks.** Servers whose data outlives any single disk. Large volumes that grow in place. Replicated
storage in [Beyond](#beyond).

**Architectures.** Both. RAID, the volume manager, and the filesystem are portable and gated under QEMU
with hot-pluggable disks in §20.9's configuration. Multipath, namespaces, and zones use QEMU's
`nvme-subsys`, `nvme-ns`, and `zoned=on` models on both architectures. The IOPS gate compares vibeOS
with Linux, the §23.6 reference kernel, in the same guest shape and job, on an NVMe namespace backed by
QEMU's `null-co` driver so the runner's disk is not what it measures: under KVM on the KVM runner, and
on aarch64 under HVF on the dev host as a §10.9 record. IOPS on data-center drives is a
[Funded goal](#funded-goals).

**Exit gate**
- [ ] RAID 1, 5, 6, and 10 arrays of four virtio-blk disks keep serving reads and writes when a member is removed with `device_del` mid-workload, and rebuild onto a replacement added with `device_add`; data checksums match afterward; under TCG with 2 vCPUs and 1 GiB, on both architectures
- [ ] 1000 simulated power cuts during RAID 5 and RAID 6 writes on §12.5's volatile-cache device, each followed by reassembly and the bitmap-bounded resync, leave no stripe whose parity disagrees with its data; 1000 more during degraded writes with the §29.2 journal leave every block the interrupted writes did not touch intact; on the weekly job in shards of at most 5.5 hours
- [ ] a logical volume and the vibefs v2 filesystem on it grow online by adding a disk to the group while a write workload runs, and host `fsck` is clean afterward, under TCG with 2 vCPUs and 1 GiB, on both architectures
- [ ] the §29.4 vibefs v2 scrub finds a block the host corrupted underneath either member of a RAID 1 array, rewrites it from the copy whose checksum verifies, and counts and logs the repair, under TCG with 2 vCPUs and 1 GiB, on both architectures
- [ ] a 1-byte write through vibefs v2 into a file block the host corrupted underneath one member of a RAID 1 array leaves the file's block holding the good copy's data with the byte written, under TCG with 2 vCPUs and 1 GiB, on both architectures (F063)
- [ ] vibefs v2 on §18.7's encryption on a logical volume on RAID 1, each member its own §12.5 volatile-cache export, passes §12.5's crash-state enumeration and content oracle, each state assembled by §29.3's host stack reader: every state mounts a committed generation at or after the last acknowledged `fsync`, under TCG with 2 vCPUs and 1 GiB, on both architectures; the §8.5 kill test cannot show a missing flush (F080)
- [ ] a namespace shared by two controllers of one `nvme-subsys` is one block device with a path through each; with a write workload running, one controller removed with `device_del` fails its path over to the other with no error reaching the filesystem, and added back with `device_add` rejoins; data checksums match afterward; under TCG with 2 vCPUs and 1 GiB, on both architectures
- [ ] on a zoned namespace (`zoned=on`), `fio` from the §14.9 mirror with `zonemode=zbd` and `verify=crc32c` passes, and `blkzone report` from the same mirror lists every zone with the write pointer the device reports, under TCG with 2 vCPUs and 1 GiB, on both architectures
- [ ] a guest with 32 virtio-blk disks on PCIe root ports and an NVMe controller with 128 namespaces boots with every disk and namespace under its §20.9 persistent name, and a RAID 10 array across the 32 disks returns what was written to it, under TCG with 2 vCPUs and 2 GiB, on both architectures
- [ ] 4 KiB random reads on the `null-co` NVMe namespace reach at least 90% of Linux's IOPS in the same 4-vCPU, 4 GiB guest shape and job, with `fio` from the §14.9 mirror on both, using the `io_uring` engine with `direct=1` (§19.8), at the same queue depth and job count; under KVM on the KVM runner, and on aarch64 under HVF on the dev host as a §10.9 record
- [ ] an NVMe drive whose critical warning is raised at runtime (QEMU's `smart_critical_warning` property) produces an event that the system log and a notification hook both see, on both architectures
- [ ] tag `phase-29` and cut the next release

### 29.1 Block layer at scale
- [ ] a mapping-target interface for stacked block devices (linear, striped, mirror, parity, thin, and §18.7's encryption transform), so RAID, volumes, and encryption stack through one mechanism; userspace drives it through Linux's device-mapper ioctls on `/dev/mapper/control` (`DM_VERSION`, `DM_REMOVE_ALL`, `DM_LIST_DEVICES`, `DM_DEV_CREATE`, `DM_DEV_REMOVE`, `DM_DEV_RENAME`, `DM_DEV_SUSPEND`, `DM_DEV_STATUS`, `DM_DEV_WAIT`, `DM_TABLE_LOAD`, `DM_TABLE_CLEAR`, `DM_TABLE_DEPS`, `DM_TABLE_STATUS`, `DM_LIST_VERSIONS`, and `DM_TARGET_MSG`, with `DM_VERSION` reporting no newer an interface version than is implemented, since libdevmapper picks its calls by it) with the `linear`, `striped`, `raid`, `thin-pool`, `thin`, and `crypt` target names, so unmodified `dmsetup`, LVM2, and `cryptsetup` from the §14.9 mirror run
- [ ] per-CPU submission kept end to end through stacked devices, onto the hardware queues of §7.2 and §20.4
- [ ] requests split at stripe and chunk boundaries, and merged below them
- [ ] arrays and logical volumes listed in §23.4's `/proc/diskstats` and under `/sys/block` with `slaves/` and `holders/` links rendered from the DESIGN §12.1 supplier links each member records, as Linux lists `md` and `dm` devices, so `iostat` and `lsblk` see the stack; per-device latency histograms, which Linux exposes only through eBPF, in debugfs under `block/<dev>/`
- [ ] native NVMe multipath: a namespace that a subsystem shares between controllers is one block device named as Linux names it, with a path per controller, I/O retried on another path when one fails, and the subsystem and its controllers under `/sys/class/nvme-subsystem`, as Linux lists them
- [ ] zoned namespaces through Linux's zoned block interface: `BLKREPORTZONE`, `BLKRESETZONE`, `BLKOPENZONE`, `BLKCLOSEZONE`, `BLKFINISHZONE`, `BLKGETZONESZ`, and `BLKGETNRZONES`, and `queue/zoned`, `queue/chunk_sectors`, and `queue/nr_zones` under `/sys/block`; writes to a sequential zone kept in order by the §7.1 request queue, and zone append where the device offers it
- [ ] §12.5's request timeouts through stacked devices: a member request that times out fails over to another path (NVMe multipath below) or fails that member of the array (§29.2), never the array, and never waits past its deadline
- [ ] arrays and volumes found by §20.9's persistent names and by filesystem UUID, never by probe order

### 29.2 Software RAID
- [ ] the member superblock, write-intent bitmap, and write journal in md's version 1.2 formats, documented in `docs/` from Linux's `md_p.h` before any code and host-tested against members that `mdadm` created under Linux, so an array assembles under either kernel
- [ ] RAID 0, 1, 10, 5, and 6; the RAID 6 P and Q arithmetic in the portable half, host-tested by recovering every pair of lost members
- [ ] a write-intent bitmap, which bounds resync after an unclean shutdown to the regions it marks, and md's write journal on a separate device, which closes the RAID 5 and 6 write hole (a power cut during a degraded write) that a bitmap leaves open
- [ ] the §12.5 volatile-cache device serving each member as its own export, with one power cut applied to all of them, which the RAID power-cut gate runs on
- [ ] degraded operation, hot spares, rebuild throttled against foreground I/O, and a rebuild that resumes after a reboot
- [ ] unmodified `mdadm` from the §14.9 mirror over Linux's md interface: `/dev/md<N>`, the md ioctls it issues, `/sys/block/md<N>/md/` (`array_state`, `sync_action`, `mismatch_cnt`, and each member's `dev-*/state`), and `/proc/mdstat`; create, assemble, add, fail, remove, and `--detail` work
- [ ] reshape: a RAID 5 or 6 grown by one member while mounted

### 29.3 Volume management
- [ ] unmodified LVM2 from the §14.9 mirror over §29.1's device-mapper interface: physical volumes, volume groups, and logical volumes in LVM2's own on-disk metadata, which the kernel never parses
- [ ] linear and striped logical volumes, extended and shrunk; online extension composed with §29.5's online grow
- [ ] thin provisioning through the `thin-pool` and `thin` targets, with discard passed down and the pool's low-water-mark event raised before it fills
- [ ] a host-side stack reader in hostlib, beside `fsck-vibefs`, that assembles a vibefs volume from member images without root, on Linux and on macOS (the hosted runners and §10.2's scheduled macOS job): RAID 1 through §29.2's portable md 1.2 code, reading the member md's resync would copy from; LVM2 linear and striped segments mapped from the physical volumes' text metadata, host-tested against volume groups LVM2 created under Linux; §18.7's transform undone through §14.7's AES-XTS and Argon2id, host-tested against volumes the kernel wrote; `fsck-vibefs` and the §12.5 enumerator take the assembled volume, which the Phase 29 host checks run on

### 29.4 Integrity and health
- [ ] scrub on a schedule and on demand: md's `check` and `repair` through `sync_action`, with mismatches counted in `mismatch_cnt` and logged; `repair` rewrites parity from data and mirrors from the first member, as Linux's does, since a mismatch alone does not say which copy is right
- [ ] a vibefs v2 checksum failure retries the read from another mirror through a per-request hint and rewrites the bad copy; a vibefs v2 scrub reads every mirror copy of every block that way, so a corrupted copy is found and rewritten from one whose checksum verifies, whichever member holds it
- [ ] a vibefs v2 partial overwrite reads its block through that verified path, so new bytes are never merged into a copy whose checksum fails and then written under a fresh checksum, as vibefs v1 did (F063)
- [ ] NVMe health logs and SMART (§20.6) polled, thresholds turned into events for the system log and a notification hook, and a failing member marked for replacement before it fails
- [ ] batched discard for SSDs through Linux's `FITRIM` ioctl, so unmodified `fstrim` trims through filesystem, volume, and RAID

### 29.5 Filesystem at scale
- [ ] vibefs v2 grown online into new space at the end of its device, without unmounting
- [ ] vibefs v2 at 16 TiB on a sparse image and at 100 million inodes, on aarch64 under HVF on the dev host as a §10.9 record, since 100 million inodes outgrow a hosted runner's 14 GB of disk: mount time, `fsck` time and memory, and lookup in a million-entry directory recorded in `docs/`; both lie far inside VIBEFS.md §15's 64-bit limits
- [ ] an in-guest test on a sparse 16 TiB vibefs v2 image writes the volume's last block and the largest file offset the format allows, and a write one byte past that offset returns `EFBIG`; 16 TiB is 2^32 blocks of 4 KiB, so its last block number is the largest 32-bit value and its block count does not fit in 32 bits
- [ ] per-user, per-group, and per-project quotas enforced and managed through Linux's `quotactl`, with project ids set through `FS_IOC_FSSETXATTR`
- [ ] writeback in parallel per filesystem and per device, measured on the gate's `null-co` NVMe namespaces under KVM on the KVM runner

### 29.6 Stretch: storage over the network, SR-IOV, and zones
- [ ] an NVMe over TCP initiator and target, so one vibeOS guest serves a namespace to another on the same runner, with multipath over two TCP connections and the failover time measured
- [ ] NVMe virtual functions (QEMU's `sriov_max_vfs`), brought up with the Virtualization Management commands and assigned to §21.2 guests through §28.4's VFIO once Phase 28 has closed
- [ ] vibefs v2 on a zoned namespace, each zone written sequentially and reclaimed whole

---

## Phase 30: Operations

**Goal.** Real server software, run unattended for a week of guest uptime, with its metrics, logs,
clock, and updates handled from off the machine.

**Unlocks.** The claim that vibeOS runs production services, backed by numbers taken against Linux in
the same guest on the same runner. [Phase 39](#phase-39-stability)'s 1.0. Fleets, cluster nodes, and live migration, in
[Beyond](#beyond).

**Architectures.** Both. The services are unmodified Linux binaries from the §14.9 Alpine mirror, on
Phase 23's surface. The comparisons with Linux run under KVM on the KVM runner, with the §23.6 reference
kernel booted over the same root in the same guest shape and job, and on aarch64 under HVF on the dev
host as §10.9 records, since GitHub's arm64 runners have no KVM. The live update gate runs vibeOS as a
host inside the runner's guest, under the Era VI nested-virtualization rule. The long run is the §25.7
soak job's carried guest. Nothing here is bought, and nothing needs Phases 26 to 29. A month of uptime on
real machines is a [Funded goal](#funded-goals).

**Exit gate**
- [ ] PostgreSQL's regression suite (§30.1) passes, minus its checked-in expected-failure list, under TCG with 2 vCPUs and 2 GiB on both architectures, on the weekly job
- [ ] pgbench, a load generator on the runner against nginx serving static files, and `redis-benchmark` each reach at least 70% of Linux's throughput in the same guest shape (4 vCPUs, 4 GiB, virtio-blk, and virtio-net with 4 queue pairs on a multiqueue tap with `vhost-net`) in the same job, under KVM on the KVM runner; and on aarch64 under HVF on the dev host as a §10.9 record, with one queue pair, since QEMU's `stream` netdev to the Era VI peer guest has only one; the numbers are recorded per release
- [ ] a host client commits numbered rows to PostgreSQL through 100 simulated power cuts on §12.5's volatile-cache device; after each, PostgreSQL recovers and every commit acknowledged to the client is present; under TCG with 2 vCPUs and 2 GiB, on both architectures
- [ ] a Prometheus server on the runner scrapes `node_exporter` on vibeOS, and every query in the checked-in dashboard returns data: CPU, memory, pressure stall, disk, network, and per-service series, on both architectures
- [ ] kernel and service logs reach a collector on the runner over TLS as RFC 5424 records with structured fields, and a sequence-number check finds none lost across a 10-minute collector outage, on both architectures
- [ ] an update whose kernel panics, one that hangs with interrupts off, and one that fails its health check each roll back with the trial boot taken through kexec (§30.4), and a good update commits through it, on both architectures under QEMU (TCG, 2 vCPUs, 2 GiB)
- [ ] with the clock's rate set 200 ppm fast through `adjtimex` before chrony starts, chrony brings its offset from an NTP server on the runner under 1 ms within 30 minutes and holds it there for an hour, in a 2-vCPU, 1 GiB guest under KVM on the KVM runner, and on aarch64 under HVF on the dev host as a §10.9 record
- [ ] a vibeOS host kernel is replaced through kexec while a 2-vCPU, 2 GiB §21.2 guest keeps its memory in place: a program in the guest finds the 1 GiB it filled before the jump unchanged after it, and a ping loop from the guest to a peer on the runner misses at most 5 s, in Phase 21's nested job on x86_64 and its HVF record on aarch64; its EL2 job runs the same on aarch64 and records the pause without a threshold
- [ ] 7 days of guest uptime on each architecture, carried across shards by the §25.7 soak job, running the §30.1 services and the §17.5 build loop in a 4-vCPU, 4 GiB guest under TCG: no panic, no watchdog reset, kernel memory and descriptor counts within 2% of day one, and, at every hourly sample taken at least 30 minutes after a shard boundary, the guest clock within 10 ms of the runner's NTP server
- [ ] tag `phase-30` and cut the next release

### 30.1 Server software
- [ ] PostgreSQL, nginx, and Redis from the §14.9 mirror, unmodified, each run as a §14.3 service entered through §14.9's `vibeos-linux` helper into one Alpine root; the calls they need are Phase 23's
- [ ] `pg_regress` from the source tarball matching the packaged PostgreSQL, run against the packaged server, with a checked-in expected-failure list whose entries each name a kernel bug or a missing feature and its box
- [ ] one script takes the vibeOS and Linux measurements back to back in one job on the same runner and guest shape, with pgbench, the HTTP load generator, and `redis-benchmark` on the runner
- [ ] results recorded in `docs/` per release, with a regression failing the scheduled job the way §19.3's thresholds do

### 30.2 Metrics
- [ ] pressure stall accounting: the time runnable threads wait for a CPU, and the time they stall on memory reclaim and on I/O, system-wide in `/proc/pressure/{cpu,memory,io}` and per §21.5 cgroup in its `cpu.pressure`, `memory.pressure`, and `io.pressure` files, in Linux's formats, with triggers written to those files and waited on with `ppoll` and `epoll`
- [ ] Prometheus's `node_exporter` from the §14.9 mirror runs unmodified over Phase 23's `/proc` and `/sys` and reports CPU, memory, pressure, filesystem, disk, and network series; each collector it cannot serve is listed in `docs/` with the reason
- [ ] per-service usage from §14.3's init and the Phase 25 error counters (machine checks, retired frames, AER, watchdog) exported through `node_exporter`'s textfile collector
- [ ] series names documented and stable across releases, with any change listed in the changelog
- [ ] the exporter's own CPU and memory cost measured and recorded in `docs/`

### 30.3 Logs
- [ ] kernel log records carry their subsystem and device as `/dev/kmsg`'s `SUBSYSTEM=` and `DEVICE=` continuation lines (§13.9), as Linux's device messages do, so rsyslog keeps them as fields rather than flattening them into text
- [ ] `/dev/log` as a syslog datagram socket feeding §14.3's system log, so ported daemons log into it unmodified
- [ ] rsyslog from the §14.9 mirror ships kernel and service logs to a host collector as RFC 5424 records over TLS, with a disk-assisted queue while the collector is away
- [ ] rotation and a disk quota for the system log, so a noisy service cannot fill the root filesystem

### 30.4 Update reboots
- [ ] kexec (§25.4) as the §22.2 trial boot when an update changes nothing the firmware loads: the new kernel starts once without touching the boot order, so any reset before the commit returns to the old entry; §25.4's `shutdown` leaves the §22.2 watchdog armed across the jump, so a hang in the new kernel still resets
- [ ] the time from the reboot request to the health check recorded for the kexec and firmware paths, per architecture

### 30.5 Time
Moved from Beyond: a server needs a disciplined clock, not a stepped one. §15.8's SNTP client still sets
the clock at boot.

- [ ] the kernel clock adjustable in rate and slewed in offset through `adjtimex` and `clock_adjtime` with Linux's `struct timex` (`ADJ_TICK` and `ADJ_FREQUENCY`, both changing the rate, `ADJ_OFFSET`, `ADJ_OFFSET_SINGLESHOT`, `ADJ_SETOFFSET` with `ADJ_NANO`, `ADJ_STATUS`, `ADJ_MAXERROR`, `ADJ_ESTERROR`, and `ADJ_TAI`, every mode chrony issues), applied to a cycle-counter-to-nanosecond multiplier and offset published through the §2.7 seqlock, which today publishes a tick count and a TSC snapshot, so a rate change takes effect from the next read without a step; the clock is computed from the cycle counter as §19.6 left it, never from a count of timer interrupts, which loses every period an interrupts-off window coalesces (F027)
- [ ] chrony from the §14.9 mirror disciplines the clock unmodified: slewing after the first sync, its drift file kept across reboots, and several servers with outlier rejection; a shipped image's chrony uses the servers `docs/NETWORK.md` (§22.2) names, and the gates point it at the runner's server
- [ ] leap seconds handled by a documented policy (smear, or step at the boundary through `STA_INS` and `STA_DEL`), tested with a simulated leap from a host NTP server

### 30.6 Live update
- [ ] a handover table passed beside the serialized `BootInfo` (§25.4), listing memory the new kernel must not touch
- [ ] §21.2 guest RAM as a shared `memory-backend-file` on a device-DAX node (`/dev/daxN.M`, as Linux names it) over a physical range reserved at boot and listed in the handover; the new kernel leaves the range out of its allocator and re-creates the same node over the same frames, so a restarted QEMU maps the pages the old one used; the option that reserves the range is Linux's `memmap=` on x86_64, and on aarch64, where Linux has none, it is listed in `docs/LINUX.md`
- [ ] `/dev/kvm`'s state get and set ioctls that QEMU's migration uses on each architecture: registers and vCPU events on both; the local APIC, I/O APIC, and KVM clock on x86_64; and the vGIC on aarch64, where the guest's time travels in the generic timer's registers because arm64 KVM has no KVM clock
- [ ] QEMU's CPR carries each guest across the jump: before `kexec -e`, QMP sets migration mode `cpr-reboot` and the `x-ignore-shared` capability and migrates vCPU and device state, without RAM, to a file; after the jump, §14.3's service manager brings back the §21.7 bridge and tap and restarts QEMU with the same command line and `-incoming` on that file, and the guest resumes
- [ ] the pause measured from the old QEMU's QMP `STOP` event to the new QEMU's `RESUME` event, and recorded per architecture

### 30.7 Long runs
- [ ] the long-run workload: the §25.7 soak workload extended with the §30.1 services and the §17.5 build loop, building from a checkout on the guest's own disk, since a host mount blocks migration, and an NTP server on the runner that chrony in the guest follows and the hourly clock sample compares against; chrony steps the clock once after each shard boundary (`makestep`), since the guest's clocks do not advance while its migration stream waits between shards
- [ ] counters sampled every minute and published per run, with §25.7's slope-based leak check
- [ ] the 7-day run repeated each month on the §25.7 soak job, in place of the weekly 72-hour soaks it overlaps

---

# Era VII. Daily Driver

A person uses vibeOS as their desktop every day, in a virtual machine, on both architectures. Era IV
proves the kernel on QEMU's models of real devices; this era proves the desktop above them. Every time,
rate, or benchmark number is compared with Linux in the same virtual machine, on the same host and in
the same job, so the host's noise cancels. Laptops, desktop machines, native display and render
drivers, and real radios, cameras, and peripherals are [Funded goals](#funded-goals); nothing here waits
for them.

**The desktop guest.** 4 vCPUs and 6 GiB: x86_64 on `q35` with OVMF and a writable variable store
(§20.9), aarch64 on `virt` with the edk2 build; a two-head `virtio-gpu-pci` with EDID, `virtio-sound-pci`,
virtio-net, virtio-blk, `virtio-keyboard-pci`, `virtio-tablet-pci`, and `virtio-multitouch-pci` (QEMU 8.1
and later), and `qemu-xhci` with `usb-kbd`, `usb-mouse`, and `usb-tablet`, plus what each phase adds. The
harness drives it through QMP `input-send-event` and `send-key`, reads each head with `screendump`, and
resizes heads through one VNC server per head (§31.7). The **Linux baseline** is Alpine's pinned
`linux-lts` kernel booting an Alpine root that holds the packages a gate runs, at the versions the vibeOS
run uses, on the same QEMU command line; kselftest and IGT comparisons use §23.6's reference kernel
instead, as Phase 23 does.

**Hosts.** A line that names no host holds in the desktop guest in two places: under KVM on the hosted
x86_64 runner, in a scheduled job, and under HVF on the dev host, as a §10.9 dev-host record. A line that
says "in CI on both architectures" holds instead in a scheduled job on the hosted x86_64 and arm64
runners, with the accelerator and guest shape it names, and needs no record. A run longer than 5.5 hours
is split into shards that carry their state as job artifacts, since a hosted job stops at 6.

The kernel speaks Linux's interface for each device class it adds here (DRM, evdev, HID, ALSA, V4L2,
nl80211, Bluetooth sockets, and their sysfs classes), so the userspace above it is upstream's,
unmodified. "From Alpine" below means a binary from the pinned Alpine release that §14.9 runs. The gates
run those binaries; Phase 37 ships the same software built from source by the Phase 24 ports tree.

Each phase lands every syscall, `ioctl`, and `/proc` or `/sys` file its gates' software uses that
Phase 23 did not, found by tracing its gates with `strace` from Alpine, each with a §13.11 differential
test where it needs no device and an in-guest test where it does. Test suites Alpine does not package
(libinput's and libevdev's, the kselftest `hid` tests with their `hid-tools`, IGT, dEQP and
`deqp-runner`, piglit, BlueZ's testers, hostap's hwsim tests, and the `glmark2` benchmark in its
`drm-glesv2` flavor) are built as §13.11 builds its corpus, from pinned upstream sources checked by
SHA-256, and are never shipped; so are the peers that run on the host, `wmediumd` and BlueZ's `btvirt`.

Every phase here needs 20, for QEMU's device models, §20.3's USB, and §20.9's firmware variables and
hotplug, and 23 for sysfs and uevents in Linux's layout, which udev and everything above it read.
Phase 31 needs nothing else. Phase 32 needs 31, and §18.5's debugfs, which carries the DRM counters and
pipe CRCs. Phase 33 needs 32. Phases 34 and 35 need 31, not 32; 34 also needs §19.4's real-time class
for PipeWire's data thread, and 35 also needs 34 for Bluetooth audio. Phase 36 needs 33 to 35, and 21
for the browser sandbox's namespaces. Phase 37 needs 36, 22 for the installer, unattended updates, and
tested-platforms list, and 24 for the ports tree. Nothing here needs Era VI, and nothing in Era VIII
needs this era.

## Phase 31: Desktop Platform

**Goal.** The desktop guest behaves like a desktop machine: suspend that survives hundreds of cycles,
keyboards, pointers, tablets, and touch that libinput drives, a power button that shuts down cleanly,
runtime power management, and firmware loaded for every device that asks. Also the harness pieces and
the Linux baseline every later gate in this era is measured with. A laptop's embedded controller,
battery, lid, and touchpad are [Funded goals](#funded-goals).

**Unlocks.** Suspending the desktop. Runtime power management that every later driver hooks into. The
firmware loader, the input injection, and the Linux baseline that Phases 32 to 37 use.

**Architectures.** Both. The firmware loader, runtime PM, the HID parser, evdev multitouch, `uinput`,
and `uhid` are shared, and libinput's and the kselftest `hid` suites run on both. S3 is x86_64 only, on
`q35` with OVMF; aarch64 suspends through s2idle alone, since QEMU's PSCI has no `SYSTEM_SUSPEND`. PS/2
is x86_64 only. The power button arrives through ACPI on `q35` and through the device tree's `gpio-keys`
node on `virt`'s PL061.

**Exit gate**
- [ ] 500 consecutive suspend cycles in the desktop guest, s2idle and S3 alternating on x86_64 and s2idle on aarch64, woken alternately by the RTC alarm and by a key sent with `input-send-event`, with no hang, the same `/sys/devices` list after every resume, and a 1 GiB fetch from the host over virtio-net with no corruption after the last cycle; and 100 such cycles in CI on both architectures (TCG, 2 vCPUs, 2 GiB)
- [ ] resume from each sleep state, from the wake event to thawed userspace by each kernel's log timestamps, takes at most 1.5 times the Linux baseline's
- [ ] `libinput list-devices` from Alpine reports each input device the desktop guest carries (the virtio keyboard, tablet, and multitouch devices, `usb-kbd`, `usb-mouse`, and `usb-tablet`, and on x86_64 the PS/2 keyboard and mouse) with the capabilities it reports on the Linux baseline
- [ ] a two-contact touch sequence sent to `virtio-multitouch-pci` as `input-send-event` `mtt` events arrives as evdev protocol-B slots, and `libinput debug-events` prints the event sequence it prints on the Linux baseline
- [ ] libinput's and libevdev's test suites pass through `/dev/uinput` in CI on both architectures (TCG, 2 vCPUs, 1 GiB), minus a checked-in expected-failure list, and `libinput replay` of the §31.2 recordings produces the events the Linux baseline recorded
- [ ] the kselftest `hid` target's hid-tools tests for the generic drivers (`test_hid_core.py`, `test_keyboard.py`, `test_mouse.py`, `test_multitouch.py`, and `test_tablet.py`), which build devices from real hardware's report descriptors over `/dev/uhid`, pass at least 90% of the cases that pass on §23.6's reference kernel, in CI on both architectures (TCG, 2 vCPUs, 1 GiB)
- [ ] every key QEMU's `send-key` names arrives as the evdev code it produces on the Linux baseline, through `virtio-keyboard-pci`, `usb-kbd`, and on x86_64 PS/2, media and volume keys included
- [ ] QMP `system_powerdown` arrives as `KEY_POWER` and starts an orderly shutdown through init, on both architectures
- [ ] on every device where the Linux baseline reaches it, an idle USB device on `qemu-xhci` autosuspends and an idle PCI function reaches D3hot, each resumes on use, and the §31.4 counters show it, from `power/runtime_status` read under both kernels
- [ ] the loader serves a named test blob from the firmware package in-guest on both architectures, and refuses a blob whose hash is not in the signed firmware package
- [ ] tag `phase-31` and cut the next release

### 31.1 Platform events
- [ ] the ACPI power button as `KEY_POWER`, through the fixed event and through a `PNP0C0C` device where the DSDT has one; on `virt`'s device tree, a PL061 GPIO driver and Linux's `gpio-keys` binding, whose `poweroff` key QEMU raises on `system_powerdown`
- [ ] general-purpose events past the fixed ones: `_Lxx` and `_Exx` methods, and wake GPEs armed before suspend
- [ ] the power button handled by a service under §14.3's init, which starts an orderly shutdown, rather than by kernel policy, logged either way; §36.1's elogind takes it over inside a session

### 31.2 Input
Gestures, tapping, palm rejection, and pointer acceleration are libinput's, from Alpine. The kernel's
part is evdev and HID as libinput and the kselftest `hid` tests expect them.

- [ ] a udev daemon from Alpine as a §14.3 service, with its `input_id` rules and hwdb, so libinput and PipeWire find devices with the properties they filter on; libinput ignores a device without `ID_INPUT`
- [ ] HID multitouch (the precision-touchpad and touchscreen usages) over USB and `/dev/uhid`, sharing §20.3's report parser, as evdev's multitouch protocol B on §16.4's nodes, with the properties libinput reads: contact slots, resolution, `INPUT_PROP_BUTTONPAD`, and `INPUT_PROP_DIRECT`
- [ ] virtio-input's multitouch device as protocol B with `INPUT_PROP_DIRECT`, and `virtio-tablet-pci` and `usb-tablet` as absolute pointers
- [ ] the power button as `KEY_POWER`, and the keyboards' media, volume, and other extra keys as their named evdev keys (`KEY_MUTE`, `KEY_VOLUMEUP`, `KEY_PLAYPAUSE`, and the rest)
- [ ] `/dev/uinput`, which libinput's and libevdev's test suites, `libinput replay`, and BlueZ's media keys (§35.5) use; the node is mode 0600, owned by root, since an injected key reaches whatever has focus, a root shell included
- [ ] `/dev/uhid` and `/dev/hidraw<N>` in Linux's layout, which the kselftest `hid` tests and BlueZ's HID over GATT (§35.5) use; both are mode 0600, owned by root, and §36.1's ACLs give the active seat's user the `hidraw` nodes its udev rules name
- [ ] `libinput record` captures of the desktop guest's input devices on the Linux baseline, driven by the §31.7 injection scripts, checked in and replayed through `uinput` in CI on both architectures

### 31.3 Suspend
- [ ] s2idle on both architectures: freeze userspace, suspend devices in dependency order through §20.2's hooks, idle every CPU, and resume on a wake interrupt
- [ ] S3 on x86_64 `q35` through §20.2's sleep path, with the same device order as s2idle
- [ ] Linux's interface: `/sys/power/state`, and `/sys/power/mem_sleep` reporting `s2idle [deep]` on `q35` and `[s2idle]` on `virt`, so suspend callers from Alpine work unmodified
- [ ] wake by timer: `/dev/rtc0`'s `RTC_WKALM_SET` over the CMOS RTC's alarm and the PL031's match interrupt, and the `CLOCK_BOOTTIME_ALARM` and `CLOCK_REALTIME_ALARM` timers, which is how the gate's cycles run unattended
- [ ] wake by input: keyboard and pointer interrupts armed as wake sources, and QMP `system_wakeup` on `q35`
- [ ] clocks across suspend: `CLOCK_MONOTONIC` excludes suspended time, `CLOCK_BOOTTIME` includes it, and the wall clock is re-read from the RTC; the cycle counter the clocks are computed from (§19.6) is re-based at resume, so no clock steps backward: QEMU resets the vCPU on a `q35` wakeup and does not preserve the TSC, and physical machines do not preserve it across S3 either; an in-guest test reads each clock before and after each of the gate's S3 cycles (F027)
- [ ] per-device suspend and resume times recorded every cycle, so a slow driver is visible

### 31.4 Runtime power management
- [ ] a runtime-PM usage count per device on the §6.1 model: an idle device suspends after a delay and I/O resumes it, with Linux's `power/control` and `power/runtime_status` files
- [ ] PCI D3hot through the power-management capability, and back to D0 on use
- [ ] USB autosuspend, with remote wakeup for HID
- [ ] counters for wakeups per second by source and runtime-PM residency per device, and a tool that prints them

### 31.5 System services
- [ ] D-Bus from Alpine as the system bus, with polkit, as §14.3 services, since NetworkManager (§35.3), BlueZ (§35.5), and the desktop's own services (§36.1) are D-Bus services
- [ ] suspend after an idle timeout as a service under §14.3's init rather than kernel policy, logged

### 31.6 Firmware
- [ ] one loader: a driver requests a blob by name; blobs for devices needed before root come from the initrd, the rest from `/lib/firmware` in Linux's layout
- [ ] firmware as its own §14.6 package, each blob's license recorded; its blobs come from `linux-firmware` and Intel's microcode repository, pinned by commit and fetched at test time, and the package is built for tests only, since §14.10's policy publishes no proprietary binary; publishing it once a funded goal brings hardware is the owner's call
- [ ] each blob's SHA-256 in the firmware package's signed file list (§14.6), checked at load; in-guest tests use a package signed with a test key that only §14.3's harness overlay trusts
- [ ] §20.1's microcode files served through it

### 31.7 Harness and the Linux baseline
- [ ] the desktop guest as one harness configuration per architecture, with the same QEMU command line for vibeOS and the Linux baseline, run under KVM on the hosted x86_64 runner, under TCG on the hosted arm64 runner, and under HVF on the dev host
- [ ] input injection through QMP `input-send-event` (keys, buttons, relative and absolute axes, and `mtt` multitouch events) and `send-key`, each input device bound to a head through its `display` and `head` properties, since QEMU routes injected events by console
- [ ] one VNC server per virtio-gpu head (`-vnc unix:<path>,display=<id>,head=<n>`), through which a hostlib client sends `SetDesktopSize` to resize a head or to disable it with a zero size, since no QMP command does; `screendump` with `head=` captures each head
- [ ] the Linux baseline: Alpine's pinned `linux-lts` and its initramfs, booting an Alpine root built from the §14.9 mirror with the packages each gate runs, in the same job as the vibeOS run and back to back with it
- [ ] each comparison script runs unchanged on vibeOS and on the Linux baseline and records both results in one format, per release, with both kernels' versions and the package versions; when a package version differs between the two runs, the comparison is reported as failed instead of run

### 31.8 Stretch: hibernation
- [ ] hibernation through Linux's interface (`disk` written to `/sys/power/state`, the mode from `/sys/power/disk`), the image written to the swap area that `resume=` on the §10.2 command line names (`resume_offset=` for a swap file), which needs §12.7's swap, restored before init, and entered through ACPI S4 on `q35`, whose DSDT has `_S4`, and by powering off on `virt`
- [ ] the hibernation image encrypted with §18.7's disk key

---

## Phase 32: Displays

**Goal.** The kernel's display core drives every output the desktop guest has: the heads of a multi-head
virtio-gpu at the modes their EDIDs offer, heads that hotplug and resize at runtime, and a virtual display
shaped like Linux's `vkms` that gives tests what virtio-gpu cannot show them: overlay planes, pipe CRCs,
timed vblanks, and writeback. Outputs come back after suspend, and IGT judges all of it against Linux.
Native display engines are [Funded goals](#funded-goals).

**Unlocks.** A desktop across several monitors. The atomic core, plane use, and CRC-checked composition
that a funded native display driver plugs into. The display half of Phase 33.

**Architectures.** Both. The atomic core, EDID and DisplayID parsing, and the §32.2 virtual display are
portable and host-tested, and virtio-gpu is shared. The suspend line takes S3 on x86_64 only, as
Phase 31 does.

**Exit gate**
- [ ] each head of a four-head `virtio-gpu-pci` (`max_outputs=4`), sized through its §31.7 VNC server, runs at the preferred mode of the EDID QEMU gives it, and the §16.1 reference-image comparison passes on each head's `screendump`, in CI on both architectures (TCG, 2 vCPUs, 2 GiB)
- [ ] IGT's KMS tests on a checked-in list (among them `kms_atomic`, `kms_flip`, `kms_plane`, `kms_cursor_legacy`, `kms_vblank`, `kms_pipe_crc_basic`, and `kms_writeback`) fail none, on virtio-gpu and on the §32.2 virtual display, that passes on §23.6's reference kernel in the same guest on virtio-gpu and on `vkms`
- [ ] a head disabled and re-enabled 100 times through its VNC server (a zero size, then a new size each time) gets its mode and its place in the layout back each time, and the card's framebuffer and buffer-object counts return to their starting values, in CI on both architectures (TCG, 2 vCPUs, 2 GiB)
- [ ] every head comes back with its mode, layout, and content after 100 suspend cycles, s2idle and S3 alternating on x86_64 and s2idle on aarch64
- [ ] a page flip on every vblank of the §32.2 virtual display at 60 Hz for 10 minutes misses fewer than 0.1% of them by its vblank counter, and no more than `vkms` misses on the reference kernel in the same guest and job
- [ ] the §16.3 compositor puts a moving window on an overlay plane and the pointer on the cursor plane of the §32.2 virtual display when the atomic check accepts them, and over 1000 frames of a scripted scene of opaque surfaces and an opaque cursor image, each frame's pipe CRC equals that of the same frame composed without planes
- [ ] the EDID and DisplayID parsers pass their host tests on the §32.1 corpus and run as `cargo-fuzz` targets on the weekly job, with no open crash
- [ ] tag `phase-32` and cut the next release

### 32.1 Display core
- [ ] §16.1's atomic commit extended to several CRTCs, planes, encoders, and connectors per device and several devices per system, the check refusing a state the device cannot scan out, with universal planes and the properties compositors read (`IN_FORMATS` with modifiers, `rotation`, `zpos`, `GAMMA_LUT`, `CTM`, `link-status`, `max bpc`)
- [ ] EDID and DisplayID parsing with a quirk table, extending §16.2's EDID line; host-tested against a corpus of real EDIDs fetched by hash at test time, and fuzzed
- [ ] a connector change from any device turned into §16.1's `HOTPLUG=1` uevent, so compositors re-probe
- [ ] vblank events and counters behind §16.1's flip-completion events, with `DRM_IOCTL_WAIT_VBLANK`, `CRTC_GET_SEQUENCE`, and `CRTC_QUEUE_SEQUENCE`
- [ ] pipe CRCs in Linux's debugfs layout (`crtc-<n>/crc/control` and `crtc-<n>/crc/data`), which IGT checks pixels with, from any device that computes them
- [ ] framebuffer and buffer-object counts per card in debugfs, which the hotplug gate reads

### 32.2 Virtual display
- [ ] a display device shaped like Linux's `vkms`, enabled by a §10.2 command-line option, since vibeOS loads no modules (a divergence listed in `docs/LINUX.md`): CRTCs timed by a kernel timer at a set refresh rate, primary, overlay, and cursor planes composed in software, a writeback connector, `GAMMA_LUT`, and a CRC of each composed frame
- [ ] its composition in the portable half, shared with §16.3's software path and host-tested against reference frames
- [ ] the comparison side: `vkms` from the §23.6 reference kernel's release, built as a module where Debian's configuration leaves it out, run by the same IGT build in the same guest

### 32.3 virtio-gpu outputs
- [ ] up to 16 heads (QEMU's `max_outputs` cap), each a connector with its EDID from `GET_EDID`, named by QEMU's `outputs` property where it is set (QEMU 10.1 and later)
- [ ] the display-change event re-reads `GET_DISPLAY_INFO` and each EDID, and a head whose size drops to zero is a disconnected connector
- [ ] a cursor plane per head
- [ ] the harness drives heads through §31.7's VNC servers; §16.1's D-Bus route stays where QEMU's D-Bus display is available

### 32.4 Compositor on the display core
- [ ] the §16.3 compositor on §32.1: overlay and cursor planes used when the atomic check accepts them, composition otherwise
- [ ] fractional scale on §16.5's per-output scale, each output's default taken from its EDID size
- [ ] output layout stored per monitor identity (the EDID's manufacturer, product, serial, and name), so a head that comes back gets its place back
- [ ] night light through the CRTC's `GAMMA_LUT` where the output has one and in composition where it has none, CRC-checked on the §32.2 display

### 32.5 Stretch: zero-copy scanout and every head
- [ ] virtio-gpu blob resources, so a guest buffer scans out without a copy, where the host QEMU has `udmabuf` or rutabaga (Homebrew's QEMU has neither)
- [ ] all 16 heads at once, each hotplugged in turn

---

## Phase 33: Graphics Stack

**Goal.** GL, GLES, and Vulkan through Mesa's software renderers, `llvmpipe` and `lavapipe` from Alpine,
unmodified inside the guest and judged by dEQP and piglit against the same Mesa release on the Linux
baseline. Under them, Linux's render interface (render nodes, GEM handles, PRIME dma-buf, syncobjs, and
sync files) on virtio-gpu and on a virtual render device shaped like Linux's `vgem`, checked by IGT, so a
funded native driver or a host with 3D plugs in without changing userspace. Native render drivers and
hardware video decode are [Funded goals](#funded-goals).

**Unlocks.** Toolkits, GL compositors, and a browser with WebGL, rendering on the CPU (Phase 36). The
render core a funded native driver builds on.

**Architectures.** Both. `llvmpipe` and `lavapipe` are Alpine's builds for each architecture, and the
render core and the §33.2 device are shared. virtio-gpu 3D (`virgl`, `venus`) is the §33.4 stretch:
QEMU's GL displays need a DRM render node, which the hosted runners do not have, and Homebrew's QEMU has
no `virtio-gpu-gl`.

**Exit gate**
- [ ] dEQP's GLES 2, 3, and 3.1 suites through `llvmpipe` and its Vulkan suite through `lavapipe`, run by `deqp-runner` with checked-in fraction and expected-failure lists, pass within 2 percentage points of the same Mesa release on the Linux baseline, with the differing tests listed; and at a tenth of that fraction in CI on both architectures (TCG, 4 vCPUs, 4 GiB), in shards of at most 5.5 hours
- [ ] piglit's GL and GLES tests on a checked-in list, through `llvmpipe` on its surfaceless EGL platform, pass within 2 percentage points of the Linux baseline, with the differing tests listed
- [ ] `kmscube` renders through GBM on `llvmpipe` and scans out on virtio-gpu, and its last frame matches a reference image, in CI on both architectures (TCG, 2 vCPUs, 2 GiB)
- [ ] `glmark2-es2-drm` on `llvmpipe` scores at least 80% of the Linux baseline's score
- [ ] IGT's `core_*`, `syncobj_*`, `sw_sync`, `prime_*`, and `vgem_*` tests on a checked-in list fail none, on virtio-gpu and on the §33.2 device, that passes on §23.6's reference kernel in the same guest on virtio-gpu and on `vgem`
- [ ] a GPU client killed mid-frame leaves no buffer, mapping, dma-buf, or syncobj behind, from the card's debugfs counts, in CI on both architectures (TCG, 2 vCPUs, 2 GiB)
- [ ] tag `phase-33` and cut the next release

### 33.1 Render core
- [ ] render nodes (`/dev/dri/renderD128` and on) with Linux's GEM handles, `mmap` offsets, and PRIME dma-buf export and import as file descriptors
- [ ] syncobjs, timeline syncobjs, and sync files, with dma-buf's `EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE`, so explicit-sync compositors and implicit-sync clients interoperate
- [ ] Linux's `sw_sync` timelines in debugfs, which IGT's sync-file tests drive
- [ ] buffers under §12.6 reclaim: purgeable buffers released under pressure, and nothing left behind when a client dies
- [ ] render nodes in Linux's sysfs and uevent layout, so libdrm's `drmGetDevices2` and Mesa's loader find every device

### 33.2 Virtual render device
- [ ] a render device shaped like Linux's `vgem`, enabled by a §10.2 command-line option as §32.2's display is: GEM buffers in system memory, mapped and exported as dma-bufs, with its fence-attach and fence-signal ioctls, which IGT's `vgem_*` and `prime_vgem` tests drive
- [ ] the comparison side: `vgem` from the §23.6 reference kernel's release, built as a module where Debian's configuration leaves it out, run by the same IGT build in the same guest

### 33.3 Software rendering
- [ ] Mesa from Alpine, unmodified: `llvmpipe` for GL and GLES and `lavapipe` for Vulkan, scanning out through GBM on the card node's dumb buffers (Mesa's `kms_swrast`), and surfaceless EGL for the test suites
- [ ] their worker threads, one per vCPU, on §13.1's threads and §13.5's futexes, and their shader JIT on §12.4's `mmap` and `mprotect`
- [ ] dEQP, `deqp-runner`, piglit, `kmscube`, and `glmark2` from pinned sources, as the era preamble says, the same builds on vibeOS and on the Linux baseline
- [ ] fraction and expected-failure lists per suite and architecture, each entry naming a reason; a listed test that passes fails the run, as §13.11's lists do, so the lists only shrink

### 33.4 Stretch: virtio-gpu 3D and compute
- [ ] Linux's virtio-gpu DRM uapi: context init with capsets, blob resources, and fenced execbuffers, so Mesa's `virgl` and `venus` run unmodified; it extends §16.2's driver through the typed, fenced buffers §16.2 left room for
- [ ] a host that renders them: QEMU's `virtio-gpu-gl`, with `venus` from QEMU 9.2, needs a host DRM render node, which the hosted runners lack, so the first try there is SDL or GTK with GL under Xvfb on `llvmpipe`; UTM or krunkit (Venus over MoltenVK) on the dev host, as records
- [ ] OpenCL through Mesa's `rusticl` on `llvmpipe`, with piglit's CL tests

---

## Phase 34: Audio and Cameras

**Goal.** Sound out and in through Linux's ALSA interface and PipeWire from Alpine, on the desktop
guest's virtio-sound, HDA, and USB audio devices, each checked from the host; and a camera through V4L2
on a virtual capture device. Hardware codecs, jacks, audio DSPs, USB Audio Class 2 headsets, and webcams
are [Funded goals](#funded-goals).

**Unlocks.** Media playback and calls (Phase 36). The audio device model Bluetooth audio joins (§35.5).

**Architectures.** Both. ALSA, V4L2, virtio-sound, HDA, USB audio, the virtual capture device, and
PipeWire are shared, and QEMU offers `intel-hda` and `usb-audio` on both. Capture goes through QEMU's
D-Bus audio backend (§34.6), which needs QEMU's D-Bus display; `-display dbus,p2p=yes`, with the client
attached through QMP `add_client`, runs without a session bus, the dev host included. On the hosted
runners both come from Ubuntu's `qemu-system-modules-opengl` package.

**Exit gate**
- [ ] in CI on both architectures (TCG, 2 vCPUs, 1 GiB): a 1 kHz tone played for 60 s through PipeWire on virtio-sound is recorded by QEMU's `wav` audiodev, and a host check finds the peak at 1 kHz ± 1 Hz and no gap over 1 ms; the same through `intel-hda` with `hda-output`, and through `usb-audio` on `qemu-xhci`
- [ ] `alsabat` from Alpine plays its tone on virtio-sound and on `intel-hda` with `hda-duplex`, the §34.6 client loops each device's playback into its capture stream, and `alsabat` finds the peak in what it records
- [ ] round-trip latency through that loopback, at PipeWire's default quantum, is within 5 ms of the Linux baseline's
- [ ] a `usb-audio` device added with `device_add` during playback takes the stream within 500 ms, and removing it with `device_del` moves the stream back, as PipeWire and WirePlumber do on the Linux baseline
- [ ] 2 hours of playback during a parallel kernel build leave no more xruns in PipeWire's counters than the Linux baseline's 2 hours in the same job
- [ ] in CI on both architectures (TCG, 2 vCPUs, 1 GiB): `v4l2-compliance` from Alpine passes against the virtual capture device (§34.5), minus a checked-in list
- [ ] the virtual capture device streams 1080p at 30 frames per second to `ffmpeg` from Alpine for 10 minutes with under 1% of frames dropped, from the V4L2 sequence numbers
- [ ] tag `phase-34` and cut the next release

### 34.1 ALSA interface
- [ ] Linux's ALSA PCM ioctls on `/dev/snd/pcmC*D*`: hardware and software parameters, and `mmap` of the DMA ring and of the status and control pages, so the hardware position is readable without a syscall
- [ ] ALSA control devices: typed elements for volume, mute, and jack state, with change events
- [ ] `/proc/asound` and `/sys/class/sound` in Linux's layout, which alsa-lib and PipeWire enumerate cards through
- [ ] underrun and overrun detection, recovery, and counters
- [ ] large buffers with an accurate position for PipeWire's timer-scheduled playback, so an idle desktop playing audio is not woken every period
- [ ] virtio-sound, with its playback and capture streams, the CI device on both architectures

### 34.2 HDA
- [ ] extends §20.6's HDA line: the codec's widget graph parsed into output and input paths from pin defaults, mixers, selectors, amplifiers, and EAPD; host-tested against the graphs of QEMU's `hda-output`, `hda-duplex`, and `hda-micro` codecs
- [ ] `intel-hda` and `ich9-intel-hda` on both architectures
- [ ] controller and codec runtime suspend under §31.4

### 34.3 USB audio
- [ ] xHCI isochronous endpoints with interval scheduling and bandwidth reservation, extending §20.3's control, bulk, and interrupt transfers
- [ ] USB Audio Class 1 on QEMU's `usb-audio`, a full-speed 48 kHz playback device, with its eight-channel mode (`multi=on`)

### 34.4 Sound server
- [ ] PipeWire and WirePlumber from Alpine on §34.1, with `pipewire-pulse` and `pipewire-jack`, so applications need no audio patches
- [ ] PipeWire's data thread in §19.4's real-time class, granted by `rtkit-daemon` from Alpine over §31.5's system bus, as on Linux desktops; rtkit grants it only to a client whose `RLIMIT_RTTIME` is set (§19.4), and a session's own `RLIMIT_RTPRIO` stays 0

### 34.5 Video capture
- [ ] Linux's V4L2 ioctls: capabilities, formats, buffer queues, `mmap`, and dma-buf export
- [ ] a virtual capture device shaped like Linux's `vivid`, with test patterns in YUYV and MJPEG, the CI device for `v4l2-compliance` on both architectures and the camera of Phase 36's call

### 34.6 Host-side audio
- [ ] a hostlib client of QEMU's D-Bus audio backend (`-audiodev dbus`; QEMU 10.0 and later, for its `nsamples` option): registered as the output listener it receives the guest's playback samples, and as the input listener it supplies capture samples; it loops one into the other for this phase's round-trip lines and plays a file into capture for Phase 36's call
- [ ] the `wav` check and the loopback measurement in the portable half of hostlib, host-tested against generated signals with known gaps and delays

### 34.7 Stretch: MIDI and a virtio camera
- [ ] the ALSA sequencer and raw MIDI, tested through a virtual MIDI device shaped like Linux's `snd-virmidi`
- [ ] virtio-media (virtio device 48) as a V4L2 driver, against rust-vmm's `vhost-device-media` capture backend through QEMU's `vhost-user-media-pci` (QEMU 11.2 and later) on the hosted runners, since vhost-user needs a Linux host

---

## Phase 35: Wireless

**Goal.** Wi-Fi and Bluetooth through Linux's nl80211 and Bluetooth sockets, so wpa_supplicant,
NetworkManager, and BlueZ from Alpine run unmodified: join WPA2 and WPA3 networks, roam, stay connected
across suspend, pair a keyboard, and stream audio. The radios are simulated and the controllers virtual,
and the peer is Linux wherever one can be: a Linux guest's hostapd on a shared `wmediumd` medium, and a
Linux guest's BlueZ on a shared `btvirt` link. Real radio chips are [Funded goals](#funded-goals).

**Unlocks.** Wireless networking in the desktop session. The nl80211 and Bluetooth stacks that a funded
chip driver plugs into.

**Architectures.** Both. The 802.11 stack, the in-kernel simulated radios, and the Bluetooth host stack
run in CI on both. The lines with a §35.4 Linux peer run on the hosted runners only, since `wmediumd`,
QEMU's vhost-user devices, and `btvirt` need a Linux host; the dev host uses the in-kernel radios and
`/dev/vhci` instead. Of those lines, the Wi-Fi join and fetch and the Bluetooth pairing run under TCG on
the arm64 runner too; the throughput, roaming, suspend, and A2DP lines are x86_64 only, because they
measure time and the arm64 runner has no KVM.

**Exit gate**
- [ ] in CI on both architectures (TCG, 2 vCPUs, 1 GiB), in a scheduled job: hostap's hwsim tests on a checked-in list pass on the §35.2 in-kernel simulated radios, covering WPA2-PSK, WPA3-SAE with hash-to-element, transition mode, protected management frames, WPA2-Enterprise with PEAP, TTLS, and TLS, fast-transition roaming, and a wrong password refused with its reason code
- [ ] in CI on both architectures (TCG, 2 vCPUs, 1 GiB): over the in-kernel medium, a station takes a DHCP lease and completes a 1 GiB fetch from the host with no corruption, and a TCP transfer completes while the station roams between two APs on one SSID
- [ ] under KVM on the hosted x86_64 runner, the desktop guest's §35.2 virtio radio and the §35.4 Linux peer's `mac80211_hwsim` share one `wmediumd` medium: through NetworkManager the vibeOS station joins the peer's WPA2-PSK and WPA3-SAE networks, takes a DHCP lease, and completes a 1 GiB fetch with no corruption at 50% or better of the throughput a second Linux peer gets as the station in the same job; and, with a TCP transfer running, it roams between the peer's two APs when the harness lowers one link's SNR through `wmediumd`'s API socket; the join and the fetch also under TCG on the hosted arm64 runner
- [ ] after each of 100 suspend cycles of the desktop guest under KVM on the hosted x86_64 runner, Wi-Fi over the virtio radio reassociates and reaches the host within 5 s of resume
- [ ] in CI on both architectures (TCG, 2 vCPUs, 1 GiB): BlueZ's `mgmt-tester`, `l2cap-tester`, and `smp-tester` pass on `/dev/vhci` controllers, minus a checked-in list
- [ ] on the hosted runners (KVM on x86_64, TCG on arm64), the desktop guest and the §35.4 Linux peer each attach an H:4 controller to one `btvirt` server: vibeOS pairs with LE Secure Connections with the peer's HID-over-GATT keyboard, keys the peer sends reach evdev through `/dev/uhid`, and after a vibeOS reboot it reconnects without pairing again
- [ ] under KVM on the hosted x86_64 runner, over the same link, vibeOS streams the 1 kHz tone as SBC through A2DP to the peer's PipeWire Bluetooth sink for 1 hour, which the peer records with no gap over 20 ms; the sink appears and vanishes as a PipeWire device on vibeOS, and the peer's AVRCP play and pause commands reach the player
- [ ] `rfkill block all` stops both radios and `rfkill unblock all` restores them, and `KEY_RFKILL` toggles the same state
- [ ] the kernel's 802.11 management-frame and information-element parsers, its hwsim message codec, and its HCI, L2CAP, and SMP parsers run as §10.2 `cargo-fuzz` targets on the weekly job, with no open crash
- [ ] tag `phase-35` and cut the next release

### 35.1 802.11 stack
- [ ] generic netlink on §15.7's netlink sockets: family registration, `CTRL_CMD_GETFAMILY`, and multicast groups
- [ ] Linux's nl80211 over it: scan, authenticate, associate, keys, station state, regulatory, and events, the interface wpa_supplicant, hostapd, `iw`, and NetworkManager speak
- [ ] a wireless device class beside §15.1's `NetDevice`, in Linux's `/sys/class/ieee80211` layout
- [ ] soft-MAC devices, such as the simulated radios: the host builds and parses management frames, runs the station and AP state machines, and does rate control; the device interface leaves room for firmware offload and for full-MAC devices behind the same nl80211 surface
- [ ] management frames and information elements parsed and built in the portable half, host-tested against captures, and fuzzed
- [ ] A-MPDU and A-MSDU aggregation with block-ack sessions, without which 802.11ac and ax rates are unreachable
- [ ] protected management frames (802.11w), which WPA3 requires, and SAE authentication frames passed to and from the supplicant
- [ ] power save, and background scans that feed roaming
- [ ] regulatory: `wireless-regdb`'s signed `regulatory.db` through §31.6, the country from the user setting and the AP, and the 6 GHz rules
- [ ] MAC address randomization while scanning and per saved network
- [ ] rfkill: `/dev/rfkill` and `/sys/class/rfkill` in Linux's layout, for Wi-Fi and Bluetooth

### 35.2 Simulated radios
- [ ] in-kernel radios shaped like Linux's `mac80211_hwsim`, with its generic netlink control family: any number of radios on one medium with configurable loss and delay
- [ ] a virtio driver for the hwsim device (virtio device 29), which carries the same generic netlink messages (`HWSIM_CMD_FRAME`, `HWSIM_CMD_TX_INFO_FRAME`) over its transmit and receive queues as Linux's `mac80211_hwsim` defines them, since the virtio specification has no section for it
- [ ] the harness attaches it to `wmediumd -u` through QEMU's generic vhost-user device with `virtio-id=29` (`vhost-user-test-device-pci`, QEMU 10.2 and later; from 9.0 to 10.1 the device cannot be created from the command line), with a per-link SNR configuration and `wmediumd`'s API socket
- [ ] hostapd and wpa_supplicant built from their pinned releases with the hwsim test configuration, and the test suite's driver run on Python from Alpine
- [ ] captures from the §35.4 Linux peer's `hwsim0` monitor interface, taken over the shared medium, replayed as host tests

### 35.3 Supplicant and network management
- [ ] wpa_supplicant from Alpine as the supplicant; not iwd, which would also need the kernel's keyrings (`keyctl`) and more `AF_ALG` algorithms than §35.5 lands for BlueZ
- [ ] NetworkManager from Alpine: saved networks with priority, autoconnect, and wired, Wi-Fi, and USB Ethernet (§20.6's CDC-ECM on QEMU's `usb-net`) connections, with DNS following the active connection; `nmcli` as the tool
- [ ] captive portal detection through NetworkManager's connectivity check against a URL the §35.4 peer serves; a shipped image sets the check as `docs/NETWORK.md` (§22.2) says, and only the gate points it at the peer

### 35.4 Linux peers
- [ ] the Linux peer: the Linux baseline with Linux's `mac80211_hwsim` over virtio and `hci_uart`, running hostapd with two APs on one SSID, dnsmasq, BlueZ, and PipeWire with its Bluetooth sink, in 1 vCPU and 1 GiB on the same host as the desktop guest
- [ ] a script on BlueZ's GATT and advertising D-Bus interfaces in the peer that presents a HID-over-GATT keyboard and sends scripted keys
- [ ] `wmediumd` and `btvirt` built from pinned sources for the hosted runners and started by the harness beside the guests

### 35.5 Bluetooth
- [ ] Linux's `AF_BLUETOOTH` sockets with HCI, the management interface, L2CAP including LE credit-based channels, and SMP with LE Secure Connections in the kernel, so BlueZ from Alpine runs unmodified above them
- [ ] Linux's `AF_ALG` sockets for `skcipher` `ecb(aes)` and `hash` `cmac(aes)`, which BlueZ's shared crypto opens in `bluetoothd` and in the emulator under `mgmt-tester`, `l2cap-tester`, and `smp-tester`; the emulator does not start without them
- [ ] HCI over a UART in H:4 framing through Linux's `N_HCI` line discipline, attached by `btattach` from Alpine, on a QEMU `pci-serial` port whose chardev is a `btvirt` server socket
- [ ] `/dev/vhci` virtual controllers, which BlueZ's testers drive, as the CI device on both architectures
- [ ] HID over GATT, which BlueZ delivers to evdev through §31.2's `/dev/uhid`
- [ ] A2DP through PipeWire's Bluetooth module with SBC, and AVRCP media keys through §31.2's `uinput`
- [ ] host tests that replay btsnoop captures of pairings between BlueZ instances over `btvirt`, taken with `btmon` in the Linux peer, through the kernel's HCI and SMP code

### 35.6 Stretch: Wi-Fi 7, access point, and headsets
- [ ] Wi-Fi 7 multi-link operation on the simulated radios, against hostap's EHT tests
- [ ] access point mode, with the Linux peer as the station, to share the desktop's connection
- [ ] HFP headset microphones over SCO sockets with mSBC, and LE Audio over ISO sockets with LC3, against BlueZ's `sco-tester` and `iso-tester`
- [ ] classic Bluetooth HID through HIDP

---

## Phase 36: Desktop Session and Applications

**Goal.** A desktop a person logs into and works in, all of it upstream Linux software from Alpine,
unmodified: Wayland compositors and a full desktop environment, GTK and Qt applications, a browser with
its sandbox, a screen reader over AT-SPI, and input methods, each measured against the same software on
the Linux baseline.

**Unlocks.** Phase 37's daily use. Desktop software without porting.

**Architectures.** Both. Everything above the kernel is Alpine's build for each architecture, rendering
with `llvmpipe` (§33.3). The call line runs a second vibeOS guest on the same host.

**Exit gate**
- [ ] in CI on both architectures (TCG, 4 vCPUs, 4 GiB), in a scheduled job: Weston on its DRM backend runs `weston-simple-shm` and `weston-simple-egl` on `llvmpipe`, and a screenshot of each matches its reference image
- [ ] the §36.2 desktop starts from its display manager, authenticating a user from §14.3's shadow file; logout and a second login work
- [ ] the session locks on suspend and after the idle timeout, each shown by logind's `LockedHint`; after a suspend, the first frame `screendump` captures from the head on resume is the lock screen; killing the locker leaves the session locked or ends it, never unlocked
- [ ] with ten windows moving at 1920×1080, the §36.2 compositor's CPU time is at most 1.2 times the Linux baseline's for the same scene
- [ ] the browser (§36.4) passes the web-platform-tests subset on a checked-in list, served from the host, within 5 percentage points of the same browser version on the Linux baseline
- [ ] Speedometer 3 in the browser, served from the host, scores at least 70% of the same browser version's score on the Linux baseline
- [ ] a 1080p30 VP9 video served from the host plays in the browser for 10 minutes, decoded in software, with at most 1 percentage point more of its frames dropped than on the Linux baseline, by the browser's own count
- [ ] a WebRTC call in the browser between the desktop guest and a second vibeOS guest (2 vCPUs, 3 GiB) on the same host, each with the §34.5 virtual camera and the §34.6 client's audio, keeps video and audio live for 10 minutes with under 1% frame loss in the browser's own statistics
- [ ] a scripted AT-SPI client lists every widget of the settings app with its role and label; with focus moved by keys sent through `input-send-event`, Orca speaks the label of each widget that takes it, checked from Orca's speech log
- [ ] an automated test drives the settings app's display, Wi-Fi, Bluetooth, sound, keyboard-layout, and user pages through AT-SPI, and each change takes effect
- [ ] a scripted pinyin sequence and a scripted romaji sequence, typed through `input-send-event`, produce the expected Chinese and Japanese text through the §36.5 input method in a GTK 4 application, a Qt 6 application, and the browser, read back through AT-SPI
- [ ] the file manager mounts a FAT32 image and an exFAT image, each attached as a `usb-storage` device on `qemu-xhci` with `device_add`, copies 1 GiB to vibefs and back with matching checksums, and ejects each safely before `device_del`
- [ ] `mpv` plays a 1080p clip in each of H.264, VP9, and AV1, decoded in software, through PipeWire, dropping no more frames than on the Linux baseline and using at most 1.2 times its CPU time
- [ ] `xterm -e 'cat > /tmp/typed'` from Alpine runs under XWayland in the §36.2 desktop, and a line typed into it through `input-send-event` lands in that file
- [ ] tag `phase-36` and cut the next release

### 36.1 Session and seat
- [ ] virtual terminals `/dev/tty1` to `/dev/tty6`, `/dev/tty0` for the active one, and `/sys/class/tty/tty0/active` naming it and waking `poll` on each switch, with Linux's VT ioctls (`VT_GETSTATE`, `VT_ACTIVATE`, `VT_WAITACTIVE`, `VT_SETMODE` with `VT_PROCESS` and its `VT_RELDISP` acknowledgements, `KDSETMODE`, `KDSKBMODE`), DRM master handoff on a switch, and `EVIOCREVOKE`, so seatd and elogind switch sessions unmodified
- [ ] a D-Bus session bus per login, and accountsservice on §31.5's system bus
- [ ] elogind from Alpine on §21.5's cgroup v2 hierarchy, since both full desktops need logind's interface
- [ ] a PAM module that verifies §14.3's Argon2id shadow hashes, so the display manager, the lock screen, and `sudo` from Alpine authenticate users unmodified
- [ ] POSIX ACLs (`system.posix_acl_access` and `system.posix_acl_default`) in Linux's extended-attribute layout on vibefs v2, tmpfs, and devfs, checked in every permission check as acl(5) describes, so the `uaccess` tag that udev rules set and elogind applies gives the active seat's user its devices without making them world-accessible; `getfacl` and `setfacl` from Alpine read and set them; an in-guest test as uid 1000 opens a mode-0600 `hidraw` node only while an ACL entry names that user, and `docs/LINUX.md`'s POSIX ACL gap (§14.8) closes
- [ ] the §18.7 disk encryption passphrase asked before root is mounted, in the configured keyboard layout

### 36.2 Compositor and desktop
- [ ] Weston and sway from Alpine on §32.1's KMS and libinput, the small compositors CI runs first
- [ ] a full desktop, GNOME or KDE Plasma, chosen by a spike that counts the kernel interfaces each is missing, and written down; it runs from Alpine unmodified, with its own display manager, settings app, and file manager
- [ ] XWayland from Alpine, so X11 applications run
- [ ] the §16.3 compositor and the upstream compositors run the same clients: the §16.7 terminal emulator and the §16.6 toolkit's test application run unmodified under Weston, sway, and the chosen desktop, since all of them speak §16.5's Wayland
- [ ] Linux's FUSE protocol on `/dev/fuse`, since the document portal and GVfs mount through it; `sshfs` from Alpine mounts a directory from the host; `/dev/fuse` is mode 0666, as on Linux, and a user's mount goes through `fusermount3` from Alpine, installed set-user-ID root, whose `allow_other` needs `user_allow_other` in `/etc/fuse.conf`; an in-guest test as uid 1000 mounts through `fusermount3` and gets `EPERM` from a direct `mount`

### 36.3 Applications
- [ ] GTK 4 and Qt 6 applications from Alpine, with file choosers, screenshots, and screencasts through the XDG desktop portals
- [ ] `mpv` on `ffmpeg` with software decode and PipeWire audio
- [ ] an image viewer, a PDF viewer, a text editor, a terminal emulator, a calculator, and an archive tool from Alpine

### 36.4 Browser
- [ ] Firefox or Chromium from Alpine, chosen by a spike that weighs sandbox requirements, upstream patch count, and each candidate's build from its Alpine recipe, and written down. Each is built in an Alpine guest with no swap and the Phase 24 build guest's 4 vCPUs and 6 GiB, under HVF on the dev host, recording its peak memory and disk; a candidate that cannot build within that memory and a hosted runner's free disk, even with recipe options such as dropping PGO or LTO, is out, and the chosen one's options are written down with it
- [ ] its sandbox enabled, on §18.6's seccomp filters and §21.5's namespaces
- [ ] compositing and WebGL through Phase 33's `llvmpipe` or the browser's own software path, video decoded in software, audio through PipeWire, and the camera through the XDG camera portal
- [ ] the harness's test CA added through a browser policy file that only §14.3's harness overlay installs, never the browser's package, since the gates serve every page from the host; the browser's policy directory joins §14.6's test-anchor check, which refuses a policy that adds a certificate

### 36.5 Accessibility and input methods
- [ ] AT-SPI2 over D-Bus from Alpine, with the GTK and Qt accessibility bridges
- [ ] Orca from Alpine with speech-dispatcher and espeak-ng, reading the settings app and the browser
- [ ] every surface of the §36.2 desktop reachable by keyboard, and its high-contrast and large-text settings in effect
- [ ] IBus or fcitx5 from Alpine over Wayland's text-input and input-method protocols, with Chinese, Japanese, and Korean engines
- [ ] the gate's UI tests drive applications through the accessibility tree, so an accessibility regression fails CI

### 36.6 Removable media
- [ ] exFAT read and write in the kernel, from the specification Microsoft published in 2019; host-tested, fuzzed like the other filesystems, and checked with `fsck.exfat` from Alpine
- [ ] udisks2 from Alpine mounting removable media for the logged-in user, under §13.9's permissions

### 36.7 Stretch: Flatpak and a second desktop
- [ ] Flatpak with bubblewrap on §21.5's namespaces and FUSE
- [ ] the full desktop §36.2 did not choose
- [ ] fast user switching

---

## Phase 37: Daily Driver

**Goal.** The desktop guest runs a scripted day every night, the numbers that decide whether someone
would keep using it are measured against the Linux baseline and published, and the desktop ships in a
vibeOS release built from source.

**Unlocks.** A claim anyone can check on a free runner or on their own machine. A daily-driver tier in
the §22.3 tested-platforms list.

**Architectures.** Both. The §37.1 run is nightly under KVM on the hosted x86_64 runner and weekly under
TCG on the hosted arm64 runner, with timeouts scaled for TCG; aarch64's measured lines are dev-host
records under HVF, as the era preamble says. On the dev host, which has no vhost-user devices or
`btvirt`, the workload uses §35.2's in-kernel radios and `/dev/vhci` in place of the §35.4 peer.

**Exit gate**
- [ ] the release image carries the §36.2 desktop as §14.6 packages built by the Phase 24 ports tree, and the §22.2 installer puts it on the desktop guest's virtio disk beside the §37.2 Alpine install; both boot afterwards through their firmware boot entries
- [ ] the §37.1 workload accumulates 24 hours under KVM on the hosted x86_64 runner and 24 hours under TCG on the hosted arm64 runner, in shards of at most 5.5 hours that each boot from the previous shard's disk image, with no kernel panic, no hang, no data loss (`fsck` clean and file checksums matching), and no crash outside the injected ones
- [ ] the §37.1 nightly run on the hosted x86_64 runner has passed on 30 consecutive nights, and the weekly run on the hosted arm64 runner in its last 4 weeks, from §10.9's run history
- [ ] 1000 consecutive suspend cycles of the desktop guest under KVM on the hosted x86_64 runner, with Wi-Fi, Bluetooth, audio, and every head working after the last
- [ ] boot to the display manager, resume to the lock screen, and a build of the vibeOS tree each take at most 1.5 times the Linux baseline's time
- [ ] three consecutive release candidates from the §22.1 release job, served from a test update channel on the host, whose key the guest trusts only through §14.3's harness overlay, update the desktop guest unattended through §22.2, and an injected bad candidate rolls back
- [ ] the §22.3 tested-platforms list gains a daily-driver tier for the desktop guest's configurations (`q35` under KVM, `virt` under TCG and under HVF), generated from the §10.9 records of the §37.1 runs and the dev-host runs, with their numbers
- [ ] tag `phase-37` and cut the next release

### 37.1 Workload
- [ ] a scripted day driven through AT-SPI (§36.5) and QMP input injection: log in, browse sites mirrored on the host, edit and save documents, play local video, hold a WebRTC call with a second vibeOS guest, copy files to and from a `usb-storage` stick attached with `device_add`, suspend and resume, roam between the §35.4 peer's access points, and add and remove a display head through its VNC server
- [ ] fault injection: random application and service kills; init restarts the services and the session survives
- [ ] per-run measurements: dropped frames, audio xruns, per-device resume time, per-process memory growth, and wakeups per second at idle
- [ ] a run of at most 5 hours, nightly on the hosted x86_64 runner and weekly on the hosted arm64 runner, with thresholds that fail the job; each run commits one record, with its result and the measurements above, to §10.9's `ci-history` branch

### 37.2 Shipping
- [ ] the Phase 24 ports tree builds the §36.2 desktop, the browser included, with the recipe options §36.4 recorded, as §14.6 packages for both architectures; Alpine's binaries stay the test oracle, not what ships
- [ ] a PackageKit backend for the §14.6 package manager, so the desktop's software center searches, installs, updates, and removes vibeOS packages with their signatures shown, and lists §22.2's updates as pending, applied, or rolled back with the reason
- [ ] the desktop's own connections in `docs/NETWORK.md` (§22.2), each set by a recipe option or a shipped configuration file to the default §22.2's record gives it: NetworkManager's connectivity check, GeoClue's location service, the browser's telemetry, studies, safe browsing, and updater (through the browser's policy file; §36.4's test CA stays in the harness's copy), the software center's ODRS reviews and any Flathub remote, and debuginfod URLs; §22.2's isolated-network run boots the desktop image through the §37.1 workload's first hour
- [ ] the Alpine install the gate installs beside, built by a harness step: the Linux baseline, booted in the desktop guest, partitions the guest's blank virtio disk into an ESP, an ext4 root, and free space, installs an Alpine root with `linux-lts` on the ext4 partition from the §14.9 mirror as §23.5 builds its root, and installs GRUB to the ESP's `\EFI\alpine` directory with a `Boot####` entry in `BootOrder` in the writable variable store the guest keeps for the §22.2 install; `grub-efi`, `efibootmgr`, and the partitioning and `mkfs` tools the step runs join the mirror's pin list
- [ ] the §22.2 installer installs into a disk's free space beside an existing GPT system: it puts its slot directories in that system's ESP, leaves every file there it did not write as it was, the `\EFI\BOOT` fallback loader included, and keeps every `Boot####` entry it did not write in `BootOrder`, when it installs and when a §22.2 trial boot commits
- [ ] full-disk encryption (§18.7) on by default when the §22.2 installer installs the §36.2 desktop

### 37.3 Records
- [ ] `llvmpipe` and browser scores, simulated Wi-Fi throughput, suspend reliability, and resume and boot times for each architecture, with the Linux baseline's and the runner's CPU model beside them, written by the jobs into a results file in the repository at each release, with history, and summarized in the release notes
- [ ] a regression past its recorded threshold fails the nightly job; a number measured under KVM is compared only with history from the same runner CPU model (§10.1)

### 37.4 Crashes and reports
- [ ] a crash reporter: a user crash leaves a §13.8 core and a symbolized backtrace, and a kernel panic in §20.1's persistent record is shown after the next boot; every report stays on the machine (the record below)
- [ ] a one-command bug report that gathers the crash data, the kernel log, and the device list into a local file the user can read, and sends nothing
- [ ] before any box sends a crash or bug report, or anything drawn from one, off the machine, an agent writes an OWNER DECISION block here stating what would leave, where it would go, and how it is redacted, and that box merges only after the owner's answer is recorded here
- [ ] each report becomes a regression test in the cheapest tier or an open box in this file, per the standing gates

**Crash reports (owner position, 2026-09-23, design review H016): nothing is posted; the design is
reopened before anything leaves the machine.** A report built from a crash can carry the user's data.
A core holds the process's memory: documents, passwords, keys. A kernel log and a device list hold
hostnames, paths, serial numbers, and network addresses. The public forge is no place for any of it.

The owner's position is that crash data is not posted publicly, and that anything ever sent is heavily
redacted first. The owner has chosen no way to send reports, so the reporter and the bug-report
command keep everything local. How a report could leave the machine, if ever, is reopened with the
owner when this section is reached, through the box above. Nothing before Phase 37 depends on it.

### 37.5 Stretch: dogfood
- [ ] a person uses vibeOS in the desktop guest under HVF on the dev host as their only desktop for 14 consecutive days; its session log shows the days, and every problem filed has a regression test or an open box in this file

---

# Era VIII. Assurance

Eras I to VII build the system and measure it. This era proves the parts the rest of the kernel stands
on, and freezes the interfaces other software stands on.

Both phases run on the free resources [How to read this](#how-to-read-this) lists: proofs and model
checks on the hosted Linux runners and the scheduled macOS job, guests on the hosted runners (x86_64
under KVM, aarch64 under TCG, and soak guests under TCG on both), and anything under HVF as a §10.9
dev-host record.

Phase 38 needs 12, 14, 18, and 19, and nothing after them, so it runs beside everything from Phase 20
on. Phase 39 needs 22, 23, 24, 25, 30, and 38. It does not need 26 to 29 or 31 to 37, which continue
beside it and after it (§39.1).

## Phase 38: Verification

**Goal.** Machine-checked proofs of the buddy allocator, both page-table formats, and each step of
frame reference counting and copy on write, over the `vibeos-core` source the kernel compiles; and
model-checked specifications of the frame reference-count protocol, the vibefs commit, TLB shootdown,
and RCU. §10.8 checks parts of this to a bound. This phase removes the bound for the proved code; the
protocol specifications are model-checked to recorded bounds and tied to the code by trace validation.

**Unlocks.** Changing proved code, such as a faster allocator or a third page-table format, with the
proof as the regression test. A trusted base the §18.8 threat model cites instead of assuming. The
proved core that Phase 39 ships as 1.0, and the proof of isolation in [Beyond](#beyond).

**Architectures.** Both. The page-table proof covers the x86_64 four-level format and the aarch64 4 KiB
granule format against one abstract map, and the aarch64 half includes break-before-make (§11.2). The
§38.5 litmus tests are aarch64 only: they check barrier sequences the aarch64 port writes by hand, and
x86_64's shootdown ordering is the IPI protocol §38.3 models. Proofs and model checks run on the host,
on Linux and macOS: the hosted Ubuntu runners and the scheduled macOS job (I1). §38.4's traces come from
guests under TCG on the hosted runners.

**Exit gate**
- [ ] `make verify` checks every Verus proof and every TLA+ specification, and passes in full on the nightly job and the scheduled macOS job; its §38.1 ladder tier runs it on every push that changes `vibeos-core` or a specification; a proof over its recorded resource limit fails like a test
- [ ] Verus checks the `vibeos-core` crate itself, in the configuration the kernel builds (no `std` feature), so every proved module is source the kernel compiles for both kernel targets with specifications and proofs erased; no transcribed or generated copy of a proved module exists, and `scripts/check_verified.py` in `make check` fails when a module `docs/VERIFIED.md` lists is not a `vibeos-core` module
- [ ] the buddy allocator (§1.1), including §19.7's per-node instances, is proved for every arena size: allocated blocks never overlap each other or a free block, a freed block merges with a free buddy, the free count equals the free lists' contents, and allocation fails only when no free block of the requested order or larger exists; for every 64-bit size and alignment, `Buddy::order_for` returns no order, or an order whose block is at least that size and at least that alignment (F103)
- [ ] the page-table code refines an abstract map from virtual page to frame, permissions, and memory type for both formats: `map`, `unmap`, `protect`, and `translate` agree with the map; no two mappings of one frame have different memory types; no frame that a kernel mapping makes executable is writable through any kernel mapping, the physmap included (§18.1); every aarch64 descriptor change that §11.2 says needs break-before-make follows it (F104, F105)
- [ ] on x86_64, the page-table proof also shows that every address space's kernel-mode root holds the kernel root's kernel-half entries, which §18.2's mapper keeps true by allocating no kernel-half PML4 slot once an address space exists, and that with §18.3's KPTI on, each user-mode root maps only the entry area from the kernel half (F101)
- [ ] frame reference counts and copy on write (§12.1, §12.3): Verus proves each refcount, reverse-map, and COW-break step against its sequential contract (§38.2), and TLC checks the §38.3 frame specification built from those steps, to its recorded bound, for these properties: `FrameRef::put` releases a unit exactly once each time its count drops to zero, and never while a count on it remains, a present PTE's included; a walk of a unit's owner finds every present PTE that maps it whenever no step is in progress; and no write through a COW mapping reaches a frame whose count is above one
- [ ] TLC checks the §38.3 specifications of the frame reference-count protocol, the vibefs commit, TLB shootdown with deferred KVA free, and RCU grace periods, each to a state bound recorded in the specification
- [ ] traces from the in-guest shootdown and RCU tests and §38.4's frame tests at `-smp 4` under TCG on both architectures, and from every crash state the §12.5 enumerator produces on a vibefs v2 volume, validate against those specifications on the nightly job
- [ ] each §38.5 litmus test passes against Arm's model on the nightly job and fails with any one of its barriers removed
- [ ] every proof and specification has a recorded mutation that `make verify` must reject, so none holds vacuously
- [ ] `docs/VERIFIED.md` lists each proved property, its tool, its bound where it has one, and what is trusted: the verifier and its SMT solver, the compiler, the `arch` assembly and trait contracts, and the hardware models; `scripts/check_verified.py` fails when a part is missing, and the §18.8 threat model links it
- [ ] tag `phase-38` and cut the next release

### 38.1 Tools and the build
- [ ] Verus for unbounded proofs of code, chosen for its linear ghost permissions over raw memory (the buddy's intrusive free lists, page-table pages); Prusti and Creusot weighed against §12.1's spike record, with the reason recorded in `docs/VERIFIED.md`; Verus proves each step of a concurrent protocol against its sequential contract: an atomic reached through the §10.8 seam is an `external_body` function whose specification is the operation's effect on its value, listed in `docs/VERIFIED.md` among the trusted assumptions (§38.2), and the interleavings belong to §38.3's specifications, checked by TLC and trace validation, with §10.8's loom models below them
- [ ] TLA+ and TLC for protocols whose state spans CPUs, the disk, and time, the frame reference-count protocol among them; Kani and loom stay where §10.8 put them, for bounded proofs and interleavings
- [ ] Verus with its Z3, the TLA+ tools, and isla-axiomatic pinned like the nightly (C1), and herd7 kept on §11.7's pin, all fetched by `setup.sh` and checked by hash, with the bump procedure in `AGENTS.md`; each runs on macOS and Linux
- [ ] `vstd` and Verus's macro crates build under the kernel's pinned nightly for the host and both kernel targets, and pass the §10.9 `cargo deny` policy
- [ ] `vibeos-core` keeps building under Verus's pinned Rust as well as the kernel's nightly: §10.1's `check_core_stable.py` has kept unstable features out of it since Phase 10, and `make verify` is where any other difference between the two toolchains shows first
- [ ] `make verify` in `make help` and in AGENTS.md's How to run, and in CI inside the §10.1 budget, recorded in DESIGN §8.6: on every push that changes `vibeos-core` or a file under `docs/specs/`, one ladder tier (not one per architecture, since it runs on the host) checks every Verus proof over the whole crate and runs TLC on the specifications that changed; the nightly job and the scheduled macOS job run it in full
- [ ] proof time per module, measured on the nightly job's hosted x86_64 runner and on the scheduled macOS job, and the resource limit `make verify` enforces, recorded in DESIGN §8.6

### 38.2 Memory core
- [ ] both page-table formats (descriptor encoding, walk, `map`, `unmap`, `protect`) and §12.1's frame metadata and reverse map are `vibeos-core` modules compiled for every target, moved there wherever Phases 11 and 12 left them in the kernel binary; only root-register writes and TLB and cache instructions stay behind the §10.3 `arch` trait
- [ ] the buddy allocator inside `verus!`, with each free block's list node owned by a ghost points-to permission; the §10.8 Kani harness kept as a cross-check
- [ ] §19.7's per-node buddies are instances of the proved allocator, not a second implementation
- [ ] the abstract page-table map and the refinement proof for the x86_64 format, then for the aarch64 format behind the same §10.3 trait
- [ ] the physmap changes (§11.2's on-demand firmware-table leaves, and the §12.1 `debug_mm` unmap of a freed frame and its remap on reallocation, which need no break-before-make because that build maps the physmap at 4 KiB) proved to change only the physmap leaves that overlap their range: every other physmap page keeps its frame, permissions, and memory type, and no physmap leaf maps device memory, which §11.2's policy requires (F104)
- [ ] frame metadata (§12.1): each refcount transition, reverse-map update, and COW-break step (§12.3) against its sequential contract, and those contracts are the actions §38.3's frame specification composes; the reverse map against the PTEs that reference each frame whenever no step is in progress; and §19.8's rule that a pinned anonymous page is never COW-shared
- [ ] where proved code calls an `arch` trait, the trait's contract is an assumption listed in `docs/VERIFIED.md`

### 38.3 Protocols
- [ ] the vibefs v2 commit (§14.8), and §25.7's intent log if v2 has one, against a disk that reorders writes between flushes, tears a superblock write, completes a write or flush with an error, and loses power at any step: the mounted tree is a committed generation at or after the last acknowledged `fsync`; at every instant, every block the superblock a mount would pick reaches is intact, including a superblock whose write reported an error but reached the media; a failed commit leaves the in-memory generation where it was, and its retry writes the slot the failed commit wrote; and after any sequence of commits, failures, and remounts, every block the allocation map marks used is reachable from the newest committed generation or a snapshot (F049, F050)
- [ ] TLB shootdown with deferred KVA free (§4.10, DESIGN §7.9), with per-CPU TLBs and paging-structure caches modelled, the per-address-space CPU set with its fenced set and clear, §18.3's flush generations, §27.5's lazy roots if that box kept them, and speculative walks through any root a CPU has loaded, over x86_64's IPI protocol and aarch64's broadcast TLB maintenance (§11.2): no CPU translates a reallocated address through a stale entry; no CPU walks a freed page-table page; no kernel stack is unmapped while a CPU still runs on it, including the exiting thread's own CPU before its switch completes; and two initiators running at once, each waiting with interrupts off and serving the other's request while it waits, both finish, with no timeout in the protocol
- [ ] RCU (§19.5, DESIGN §2.12): no grace period ends while a reader that began before it is still inside, including a reader preempted in its section and on the blocked-reader list, and readers on CPUs that entered tickless idle or went offline mid-grace-period (§19.6); and no counted object is freed until its count has reached zero and a grace period has passed after that
- [ ] frame reference counts, the COW break, and the reverse map (§12.1, §12.3) across CPUs and address spaces, each action one step whose contract §38.2 proves: `FrameRef::put` frees a unit exactly once, and never while a PTE, a reverse-map entry, or another `FrameRef` names it, nor before the §12.3 shootdown round that dropped its last PTE has completed; no write through a COW mapping reaches a frame whose count is above one; and a COW fault racing §12.6 reclaim's reverse-map unmap on another CPU loses no reference
- [ ] each specification under `docs/specs/`, naming the DESIGN or VIBEFS section it formalizes, and that section linking back

### 38.4 Trace validation
- [ ] the §10.7 flight recorder and the §19.1 tracepoints record the events each specification's actions name, exported in a form the specification's trace module reads under TLC
- [ ] the in-guest shootdown and RCU tests, the frame tests (§12.3's reverse-map unmap test run at `-smp 4`, and a COW race in which a parent and its child write-fault the same 64 COW frames from two CPUs, 10,000 rounds), and the §12.5 enumerator on a vibefs v2 volume produce traces checked against the specifications on the nightly job; the frame events are recorded only while the frame tests run, since a trace of every page fault would outgrow what TLC checks
- [ ] a trace the specification rejects fails its test and prints the first step the specification does not allow

### 38.5 aarch64 barrier sequences
- [ ] litmus tests for the barrier sequences the aarch64 `arch` code writes by hand: the §11.2 PTE update with TLB maintenance, break-before-make, and the ASID rollover flush
- [ ] each checked against Arm's translation-aware model, herd7's `aarch64.cat` with its VMSA variant (isla-axiomatic where herd7 cannot express a test), and each shown to fail with any one of its barriers removed
- [ ] each litmus test, §11.7's included, records a hash of the `arch` function it models, and §11.7's `make check` guard fails when the function changes and the test does not; a helper that cites an Arm ARM rule under §11.7 names its test here instead once one covers its sequence

### 38.6 Stretch: proofs past the memory core
- [ ] the vibefs commit path proved in Verus to refine the §38.3 specification, closing the gap trace validation leaves
- [ ] the code under the §10.8 loom models run under Miri's GenMC mode, which explores weak-memory outcomes loom does not produce
- [ ] the syscall dispatch proved to pass every user pointer argument through the §10.6 accessors

---

## Phase 39: Stability

**Goal.** A 1.0 that later releases do not break. The interfaces other software depends on are frozen
and tested against every commit, supported releases get fixes, and releases come on a calendar.

**Unlocks.** Software built outside the tree that keeps working across upgrades. Releases on a calendar
as well as at phase exits. A base that the rest of Eras VI and VII, and the [Beyond](#beyond) list,
build on without moving it.

**Architectures.** Both. Every corpus holds binaries and images for each, and every supported release
is tested on each: x86_64 under KVM on the hosted x86_64 runner, and aarch64 under TCG on the hosted
arm64 runner. A soak is the §25.7 soak job's guest, under TCG on both architectures and carried across
shards of at most 5.5 hours by the Era VI long-run rule, since a hosted job stops at 6; unbroken runs on
physical machines are [Funded goals](#funded-goals).

**Exit gate**
- [ ] `docs/STABILITY.md` names the §39.1 stable set with a version for each interface, and lists every other user-visible interface as unstable with the phase expected to settle it; `scripts/check_stability.py` in `make check` fails when a part §39.1 names is missing
- [ ] `make check` compares the generated syscall table (§10.5) with the copy frozen at the `v1.0.0` release candidate, and fails on a removed entry or a changed argument layout; a host test fails when a number the table marks implemented reaches the dispatch's `ENOSYS` arm, or a number it does not mark implemented reaches a handler, so the frozen copy is the ABI the kernel serves (F150)
- [ ] the §39.2 ABI corpus runs unchanged on every push to `main`, on both architectures
- [ ] vibefs and FAT images and §14.6 packages written by the release candidate pass the §39.2 checks on `main` in CI
- [ ] an unattended A/B update (§22.2) from each supported release to `main` boots and passes `make test-e2e`, nightly, on both architectures under QEMU with OVMF and the aarch64 edk2 build (TCG, 2 vCPUs, 2 GiB)
- [ ] the release candidate accumulates 168 hours of guest uptime on each architecture running §30.7's long-run workload, in a 4-vCPU, 4 GiB guest under TCG carried across shards by the §25.7 soak job (a shard is rerun only under §25.7's rule for a job that ended without a harness verdict), and passes the checks of the Phase 30 gate's 7-day line, with §25.7's slope check projecting under 1% growth over 30 days for frame, heap, slab, and descriptor counts and each vibefs volume's used blocks and used inodes (F021, F049)
- [ ] `v1.0.0` is built on vibeOS by the toolchains Phase 24 bootstrapped from source, and, before signing, a second vibeOS build reproduces every artifact byte for byte
- [ ] the release candidate completes Phase 22's release-candidate fuzz campaigns with no crash, and no `fuzz-crash` issue (§39.3) has been open more than 14 days
- [ ] the newest §22.5 drill record is dated within 12 months before the tag and names every supported branch, and the §22.5 `make check` script passes
- [ ] every configuration `docs/HARDWARE.md`'s model list marks as tested nightly (§39.4) passed its last run, which is at most two days old
- [ ] tag `phase-39` and release `v1.0.0`

### 39.1 Interface freeze
- [ ] the Linux surface in the stable set, per architecture: the calls, flags, and `ioctl`s vibeOS implements, as the §10.5 table and the §13.9 `ioctl` registry list them, their errno values, the initial stack and auxv (§13.10), signal frames (§13.8), the `/proc` and `/sys` files of §13.10 and Phase 23, and the §16.1 DRM/KMS and §16.4 evdev nodes; for these, stable means they keep matching Linux
- [ ] vibeOS's own formats in the stable set: the vibefs v2 on-disk format (§14.8), the §14.6 package, repository, release-manifest, and key-set record formats, their version, expiry, and root-set fields included, and the documented §10.2 command-line options
- [ ] each Wayland extension vibeOS defines (§16.5) in the stable set with a version; the upstream Wayland protocols keep their own stability rules and are listed with the versions implemented
- [ ] `make gate PHASE=N` (§10.9) fails while `docs/STABILITY.md` lists an interface as unstable pending phase N, so a phase that closes after `v1.0.0` moves the interfaces it settles into the stable set, or names the later phase that settles them, in the commit that closes its gate
- [ ] a stable interface changes only by a new version served beside the old one, or by a major version under the §22.1 policy
- [ ] a stable call, flag, or file is removed only after one release that logs its use once per process
- [ ] each release checks in the list of conformance cases it passed (§13.11, Phase 23); `make check` fails when an expected-failure list names a case on the last release's list

### 39.2 Compatibility corpora
- [ ] an ABI corpus built at the release candidate and at each release after it, for both architectures: static binaries from the §10.5 runtime and from musl, dynamic binaries against musl (§14.9) and glibc (Phase 23), and Rust programs built with `std`; kept as release assets and run by a ladder tier on every push
- [ ] vibefs and FAT images from each release mounted, read, written, and checked with `fsck` on `main`
- [ ] packages and a signed repository from each release installed, upgraded, and removed on `main`, and `main`'s updater, starting from each release's key-set record, follows the chain of key-set records to the current one
- [ ] a corpus failure fails the push like any other test; a deliberate break carries a §22.1 major version bump in the same commit

### 39.3 Releases after 1.0
- [ ] after `v1.0.0`, releases are `v1.<m>.0` until a §22.1 major version bump: a phase that closes cuts the next one, and when eight weeks pass without one, a scheduled job opens a `release due` issue, and the owner tags green `main` and dispatches the release (§10.1); every release run, a patch release's included, waits for the owner's approval of its `sign` job (§14.6)
- [ ] the latest two minor releases supported: security and data-loss fixes backported and shipped as `v1.<m>.<p>`, and each supported branch running the full ladder and `make verify` nightly, and the §23.6 suites on the weekly schedule `main` uses, on both architectures; GitHub runs scheduled workflows only on the default branch, so `main`'s scheduled workflows start the supported branches' runs through `workflow_dispatch`, staggered inside §10.1's scheduled share of 10 concurrent jobs (at most 5 of them macOS), which they share with `main`'s own scheduled workflows, the release soaks below, and Phase 22's release-candidate fuzz campaigns, so pushes keep the other 10; a job that finds the share full waits for a slot in the order it was requested, and DESIGN §8.6 records the schedule and each run's peak job count
- [ ] backports by `scripts/backport.py`: `git cherry-pick -x`, and the fix's regression test must fail on the branch without the fix and pass with it, or the backport is refused; a change to the release build interface (`make release-artifacts` and the files it runs, §22.1) proves itself instead by a key-free dry run on the branch after the backport: `release.yml` dispatched with its `dry_run` input, which stops before any key job, whose `build` job and reproducibility comparison pass
- [ ] each later release candidate, and each patch release on a supported branch, passes a 72-hour soak of §30.7's workload on the commit being released before it is cut, run and checked as the exit gate's 168-hour run is; each commit's soak is its own chain of §25.7 shards, so soaks of different commits run side by side, each shard waiting its turn for a slot in §10.1's scheduled share
- [ ] a Phase 25 crash record from a supported branch's nightly or soak is filed as an issue against that branch, with the dump attached
- [ ] every scheduled fuzz job (§10.2, §13.13, §15.10, §18.5, §21.8, and each later one) files a new crash as a `fuzz-crash` issue with its seed or reproducer, and the release workflow refuses to cut a release while one has been open more than 14 days
- [ ] the release workflow refuses to cut a release when the newest §22.5 drill record is more than 12 months old or does not name every supported branch
- [ ] the end of a release's support announced one release ahead in the release notes

### 39.4 Tested configurations
- [ ] `docs/HARDWARE.md`'s model list (§20.1) marks each §20.8 profile, per accelerator, as tested nightly by the §20.8 `hardware-models` workflow or as tested by a dev-host record (the profile under HVF on the Apple Silicon dev host, §10.9), with the release it last passed at; a configuration that passed at neither of the last two releases is marked untested
- [ ] the release workflow refuses to cut a release while a nightly configuration's last run is red or more than two days old
- [ ] each release's notes carry §22.3's tested-platforms list, with each configuration's last pass read from the §20.8 records on §10.9's `ci-history` branch

### 39.5 Stretch: long-term support
- [ ] one release a year designated long-term and supported for two years, with its own nightly ladder on both architectures

Then keep going: there is no version of this where the work is finished.

---

# Beyond

Not phases. No gates, no tags. Things that are hard, well specified, and welcome as soon as the phase
that enables them is closed. Each names that phase. Take one when the queue is empty, and move it into a
phase with a gate before starting it. Every entry runs on the free resources in
[How to read this](#how-to-read-this); where an entry has a version that needs bought or rented
hardware, a paid service, or a new account, that version is in [Funded goals](#funded-goals).

- **riscv64 as a third architecture** (after §11.8 and 20): the §11.8 port carried through the Phase 20 lines that QEMU's riscv64 `virt` machine can run (its PCIe, NVMe, xHCI, and virtio models), under TCG on the hosted runners, since no free host runs riscv64 natively. A board is a funded goal.
- **Whole-kernel deterministic simulation** (after 15): the portable kernel run on the host against a simulated `arch` (§10.3), one simulated CPU at a time chosen by a seed at every lock, atomic, and interrupt point, in virtual time, with §12.5's disk faults and §15.10's faulty link under §15.1's simulated clock, every failure replayable from its seed. §15.1 and §15.10 already do this for the network stack alone, and §12.5 enumerates vibefs crash states.
- **Live kernel patching** (after 18): a fix applied to a running kernel without a reboot, with the KASLR and W^X story intact.
- **A `std`-native Rust userspace** (after 24): coreutils, shell, and init moved from the §10.5 `no_std` runtime to Rust's `std` for the `*-unknown-linux-musl` triple (§24.3).
- **Real-time latency bounds** (after 19): a scheduling class that documents its bound on interrupt and scheduling latency, with the bound measured in a KVM guest on the hosted x86_64 runner beside Linux built with `PREEMPT_RT` in the same VM shape and job, and under HVF on the dev host as a record. The bound on bare metal is a funded goal.
- **A network filesystem client** (after 15): NFS or 9p over TCP, so several vibeOS guests on one hosted runner share one tree served from the runner host.
- **A WASM runtime** (after 14): a sandbox that is not a process.
- **Cross self-hosting** (after 11 and 17): aarch64 vibeOS builds x86_64 vibeOS and the reverse, byte-identical to the native build.
- **vibefs v3** (after 19): a log-structured or journaled design measured against v2's copy-on-write metadata on the §19.3 file benchmarks, with an upgrade path from v2.
- **Record and replay** (after 10): `make record` boots a ktest ISO under QEMU record/replay (`-accel tcg,thread=single -icount shift=auto,rr=record,rrfile=<path>`, since the ktest tiers run at `-smp 2` and `-smp 4`) with disks through `blkreplay` and a fixed `-seed`, and `make replay` replays it under the §10.1 `make debug` gdb script with `reverse-stepi` and `reverse-continue`; a ladder failure in CI is rerun once under record for diagnosis, the job stays red whatever the rerun shows, and a recording that reproduces it is uploaded. Then the recordings indexed into a database of memory writes, register states, and control flow, so an agent asks which instruction last wrote an address instead of rerunning with prints.
- **CHERI capabilities** (after 11 and 18): a port to CHERI-RISC-V or Arm's Morello architecture under the CHERI QEMU, with every kernel and user pointer a bounded capability. The first deliverable is whether a CHERI-capable Rust compiler is usable. A Morello board is a funded goal.
- **Gate replay** (after 13): each Era I and II phase replayed as a benchmark. A fresh agent team starts from the commit that closed the previous phase, with only that phase's section and `AGENTS.md`, and `make gate PHASE=N` (§10.9) scores the result; rerun when a new model ships. Phases 0 to 9 first get gate-map entries written against their closing commits. It runs in the maintainer's own agent sessions and spends their tokens, so it starts only when the maintainer asks for it.
- **eBPF** (after 19): Linux's `bpf(2)` with a verifier and a JIT on both architectures, attached to §19.1's tracepoints, so unmodified `bpftrace` one-liners run; the verifier fuzzed like every parser.
- **Hypervisor record and replay** (after 21): the §21.1 hypervisor logs a guest's exit results, interrupt injection points, and device completions, and replays the guest deterministically under gdb's reverse execution at the hypervisor's speed rather than TCG's, one vCPU first, on the hosts Phase 21's gate runs on.
- **Live migration** (after 21 and 30): a running §21.2 guest moved over TCP by QEMU's migration, on §30.6's `/dev/kvm` state ioctls, between two vibeOS hosts that are KVM guests on one hosted x86_64 runner, nested (which GitHub calls experimental; each run uses the VMX or SVM path its CPU offers), with dirty pages tracked through EPT or NPT dirty logging and pre-copied, and the downtime of a 1-vCPU, 1 GiB guest running §19.3's mixed interactive workload measured beside two Linux KVM hosts in the same VM shape in the same job; on aarch64 the same under TCG with `virtualization=on` and stage-2 dirty logging. Migration between physical machines is a funded goal.
- **A Kubernetes worker node** (after 21 and 30): the upstream kubelet with a CRI runtime over §21.6's OCI images and a CNI plugin over §21.7's virtual networking, in a vibeOS guest joined to a k3s control plane on the hosted runner; the upstream node conformance suite becomes the gate when it moves into a phase.
- **Agent performance ledger** (after 22): which agent and model did what, from commit trailers, and per phase the slices, pull requests, days from first slice to tag, red CI runs, reverts, reopened boxes, and escaped bugs by the tier that should have caught them, computed from git and GitHub history into the release notes. The maintainer deferred this ([ARCHITECTURE_REVIEW](reviews/ARCHITECTURE_REVIEW.md) O1), so it starts only when the maintainer asks for it.
- **Agents on vibeOS** (after 23): the coding agent that develops vibeOS runs in a vibeOS guest under HVF on the dev host through the Linux ABI (Node.js passes Phase 23's suite), reaches the model API over HTTPS through Phase 15's stack, and lands one slice from there. It runs on the maintainer's own agent account and spends their tokens, so it starts only when the maintainer asks for it.
- **Foreign-architecture binaries** (after 23): `binfmt_misc` and an unmodified static `qemu-user`, so aarch64 vibeOS runs x86_64 Linux binaries and the reverse, tested by running LTP that way.
- **Debian with systemd** (after 21 and 23): an unmodified Debian install boots on the vibeOS kernel with systemd as PID 1 on §21.5's cgroup v2 hierarchy, udev, and journald, and Debian's `autopkgtest` runs over a package set with a recorded pass rate.
- **Full-source bootstrap** (after 24): a chain from a few-hundred-byte `hex0` seed to the §24.1 C toolchain, as stage0-posix and live-bootstrap build one, run on vibeOS through its Linux ABI, so Alpine's clang and lld leave §24.5's seed list. vibeOS runs only the chain's 64-bit ports (no 32-bit user ABI), and whether they reach GCC on both architectures is the first deliverable.
- **CXL memory tiering** (after 27): CXL Type 3 memory as a far §19.7 NUMA node, with pages promoted and demoted by measured access, against QEMU's CXL emulation under TCG. CXL hardware is a funded goal.
- **Software RDMA** (after 28): RoCEv2 through Linux's verbs ABI as a software device over virtio-net, as Linux's `rxe` does, exchanging traffic with `rxe` in a §23.6 reference-kernel guest on the same runner, so unmodified `rdma-core` runs; then NVMe over RDMA on it. RoCE on a hardware NIC is a funded goal.
- **Replicated block storage** (after 29): a volume mirrored synchronously over TCP to a second vibeOS guest on the same runner, with failover and resync time measured.
- **Fleet rollouts** (after 30): a control plane that enrolls vibeOS guests (eight at 1 GiB each on one hosted runner) and rolls an update across them in waves, each wave gated on the §22.2 update health check and Phase 30's metrics, halting on a regression.
- **Media-controller cameras** (after 34): Linux's media controller API with a virtual camera pipeline shaped like Linux's `vimc`, run by unmodified `libcamera` through its `vimc` pipeline handler and checked by `v4l2-compliance`. MIPI cameras behind Intel's IPU6 are a funded goal.
- **Printing** (after 36): IPP Everywhere, which most network printers sold in the last decade speak, through CUPS from Alpine, printing to CUPS's `ippeveprinter` on the runner host. Scanning and a physical printer are a funded goal.
- **Proof of isolation** (after 38): a machine-checked statement that no syscall sequence lets one process read or write another's memory, built on Phase 38's page-table and frame proofs and the syscall dispatch.

---

# Funded goals

vibeOS has no budget. This is where money, a machine, or an account would go. Groups are in priority
order and so are the goals within each: the first purchase that unlocks the most comes first. Nothing in a
phase depends on anything here, and no gate waits for it.

Each goal names what to buy, rent, or open, a rough cost with the year it was estimated (or, where no
price is published, that fact and the year), what it unlocks, and the lines it adds back. Costs are
estimates to confirm before buying. When a goal is met, one edit to this file moves its `- [ ]` lines
unchanged into the places named above them, makes its other edits, drops it from the sentences that list
funded goals, and deletes the goal. A line for a phase already tagged lands as an open box; the tag
stands. With the first goal met, that edit also changes the Free by default paragraph in How to read
this: "beyond their own agent tokens" gains "and the funded goals already met", and after "A gate never
needs a physical machine, a rented one, a paid service, or a new account" it adds "Lines moved in from a
met funded goal are the exception: each names the machine, account, or service it needs, and every
statement in this file that a phase, era, section, gate, or Beyond entry runs on the free resources,
runs only on hosted runners or QEMU, assumes no physical machine, or buys nothing excludes them". This
section's "vibeOS has no budget" becomes "vibeOS has no budget beyond the goals already met".

A machine the maintainer owns for other reasons can take a goal's lines without buying anything, by the
same edit, when it meets the goal's specification. The maintainer's own Mac is not such a machine: it
stays a VM host, since an installer or boot-policy bug there costs the dev host.

Free cloud tiers are here, not in phases. They cost nothing while use stays inside them, but each needs
an account and a card the maintainer opens, and some can start billing.

**Self-hosted runners.** Several goals register a self-hosted GitHub Actions runner to this repository.
GitHub recommends self-hosted runners only for private repositories: a runner takes any job that names
its labels, and a pull request from a fork can add a workflow that does, which GitHub runs from the pull
request's own files. A runner here therefore enforces its rules itself: its job-started hook
(`ACTIONS_RUNNER_HOOK_JOB_STARTED`) fails every job whose `GITHUB_EVENT_NAME` is not `schedule` or
`workflow_dispatch`, or whose `GITHUB_REF` is not `refs/heads/main`, before any of its steps runs, and
vibeOS's own workflows that name its labels have no other trigger. A `workflow_dispatch` run builds a
commit it takes as input only when that commit is on `main` or on a §39.3 supported branch. The runners
run nothing but vibeOS's hardware jobs. Each job runs in a fresh VM on the rig host, registered as an
ephemeral just-in-time runner and deleted after its job; the VM's network reaches GitHub and the rig's
management network, never the owner's LAN; the rig host holds no secret but that registration, and a job
switches outlets or netboots a machine only through a service on the rig host with an allow list of
outlets and a rate limit. The maintainer signs these rules off when funding the rig (DESIGN §2.10, code
under test).

**Secure Boot on physical machines.** A funded line that installs vibeOS on a physical machine keeps
Secure Boot on. The vibeOS db certificate is added through the firmware's key menu beside the maker's
and Microsoft's entries, which stay, so another system's Microsoft-signed shim, such as Fedora's on the
reference laptop, still boots. Where the firmware cannot add a db entry, vibeOS boots with Secure Boot
off and §18.7's check says `boot: unverified`. A Microsoft-signed shim needs a paid code-signing
certificate and is not planned.

**Names.** In the lines below, *the x86_64 test PC*, *the aarch64 server*, *the long-run servers*, *the
reference laptop*, *the desktop*, and *the rig* are the machines the goals buy. *The test machines* are
the x86_64 test PC and the aarch64 server. *The rig host* is the machine that controls their netboot,
serial capture, and switched power, bought with the x86_64 test PC, and it runs the self-hosted runners.

## Bare metal and hardware CI

### Used x86_64 test PC

**Buy.** A used x86_64 PC with a serial port, then a second, physically different one. Both: VT-x or
AMD-V, and VT-d or AMD-Vi with SR-IOV, enabled in firmware; UEFI with a GOP framebuffer; USB ports and an
internal NVMe or SATA disk. At least one with S3 in its firmware. The first also has at least 8 cores, a
free CPU-attached PCIe 3.0 or newer x16 slot, and a second NVMe slot, so it can carry the NVMe-drive and
100GbE goals; an Intel CPU is the simpler choice for `rr`, which needs a workaround on AMD Zen. With them:
real NVMe and SATA drives, a Realtek 8168 NIC, a USB-C dock or adapter with CDC-NCM and an ASIX AX88179,
USB serial adapters (CDC-ACM, Prolific PL2303, Silicon Labs CP210x), a switched outlet per machine, and a
plug-in power meter. For the rig, the rig host: an always-on x86_64 mini PC running Linux, with VT-x or
AMD-V so it can run a vibeOS guest under KVM, gigabit Ethernet, and a powered USB hub for its own serial
adapters and the rig's injectors; it serves netboot, captures serial, switches the outlets, and runs the
self-hosted runners.

**Cost.** About $1,500 for both test PCs and the parts above, and about $300 for the rig host (2026
estimates). One test PC alone is roughly half the $1,500 and, with the rig host, closes every line below
but the second-machine clauses.

**Unlocks.** Bare-metal boot on real UEFI and BIOS firmware, with real memory maps, MTRRs, ACPI tables
and their quirks, and microcode that changes the revision. S3 on real firmware, and idle power measured
at real C-states. Hardware PMU events: cache and branch misses, LBR, PEBS, and IBS. Physical drives'
error paths and SMART, and the NICs and USB adapters QEMU does not model. The hypervisor on real VMX or
SVM, free of GitHub's experimental nesting and its per-job vendor draw. A live image and an install on a
physical machine, and the first physical rows of `docs/HARDWARE.md` (§20.1) and of §22.3's
tested-platforms list. A self-hosted hardware-CI runner (see **Self-hosted runners**). With ECC, EINJ,
ERST, GHES, and a BMC, it can also be the x86_64 long-run server; it can be the desktop of Era VII.

**Lines.** Elsewhere in this file:
- the destination paragraph: "boots on QEMU's models of real machines on both architectures" gains "and on a real x86_64 machine"
- How to read this, the conditions paragraph: "and the VMM when it is not QEMU" gains "; on real hardware, the machine"
- the standing gates' gate-map line: "a CI job on GitHub-hosted runners" becomes "a CI job on GitHub-hosted runners or on a self-hosted runner on the rig host whose workflow's only triggers are `schedule` and `workflow_dispatch`", and the record clause also covers "a reading taken by hand on a physical machine, such as a power meter's"
- the arc, row 20: Unlocks gains ", bare metal on x86_64, hardware CI"
- §10.9, the gate-map box: "a scheduled CI job on GitHub-hosted runners" becomes "a scheduled CI job on GitHub-hosted runners or on a self-hosted runner on the rig host"
- §10.9, the `make gate` box: "or a job entry names a workflow whose `runs-on` has a `self-hosted` label" becomes "or a job entry names a workflow whose `runs-on` has a `self-hosted` label and whose `on:` has a trigger other than `schedule` and `workflow_dispatch`"
- §10.9, the dev-host records box: "with `gh workflow run` on a branch at that commit" gains "(for a workflow on a self-hosted runner, on `main` while `main` is at that commit)", and the box gains "A reading taken by hand on a physical machine, such as a power meter's, is a record too: `make gate PHASE=N RECORD=1` writes it to `ci-history` with the machine and the instrument in place of the host and the command"
- the Era IV preamble: "deeper C-states and frequency scaling from ACPI extend the §19.6 idle path" becomes "deeper C-states, frequency scaling from ACPI, and measured power extend the §19.6 idle path", and "three sections of 19" becomes "four sections of 19", with "§20.8's hardware-event profiles use §19.2's sampling" added to its list
- Phase 17 exit gate, the line on `make check` and `make test` passing on vibeOS, gains "; the run on bare metal is Phase 20's"
- Phase 19 Architectures: hardware events "need a physical machine and are in [Funded goals](#funded-goals)" becomes "are validated on the x86_64 test PC in §20.8"
- §19.2, the PMU setup box: "which no free host offers a guest ([Funded goals](#funded-goals))" becomes "counted on the x86_64 test PC booted bare metal (§20.8)"
- §19.6, the idle box: "measured power needs a physical machine ([Funded goals](#funded-goals))" becomes "measured power is §20.2's, on the x86_64 test PC"
- §22.3, the tested-platforms box: "its entries are virtual machine configurations, and it claims no physical machine" becomes "its QEMU entries are virtual machine configurations, and physical machines are in its physical section"

§10.9:
- [ ] `scripts/check_gates.py` accepts a job entry whose workflow runs on a self-hosted runner only when that workflow's only triggers are `schedule` and `workflow_dispatch`, and `make gate PHASE=N RECORD=1` records a hand reading on a physical machine, each with a host test

§18.3:
- [ ] the Phase 18 exit gate's measured-cost entries include the x86_64 test PC's CPU model booted bare metal, and the harness test that compares the boot log's mitigation list with the document's list runs there (F024, F131)

Phase 20 exit gate, before the tag line:
- [ ] boots from USB on the x86_64 test PC, and on a second, physically different x86_64 machine once one exists, with output on a serial adapter or the screen
- [ ] on each of those machines: Phase 7's pattern and concurrent read-write tests pass on a scratch partition of its internal disk; a TCP client fetches 1 GiB from a peer through vibeOS's driver for its NIC with no corruption, as the Phase 15 gate does over virtio-net; and `evtest` reads a key typed on a USB keyboard from its `/dev/input/event<N>` node
- [ ] the AML interpreter loads the DSDT and SSDTs of each of those machines, host-tested against their `acpidump` output, which joins the §20.2 corpus, and `_PRT` resolves PCI interrupt routing on each
- [ ] S3 suspend and resume on the machine whose firmware offers S3, with its disk, NIC, and USB keyboard working afterward
- [ ] on each of those machines, `poweroff` enters S5 through its FADT's PM1 control blocks and `_S5`, read on the plug-in power meter as the machine's soft-off draw, and `reboot` restarts it through the FADT reset register, each after the log line naming the path it takes (F097)
- [ ] idle power measured with the plug-in power meter on each machine at the shallowest and deepest C-state §20.2 enables, with tickless idle on and off, the numbers in `docs/HARDWARE.md`'s physical section
- [ ] NVMe and AHCI drives detected and used as root on those machines
- [ ] `make test` passes nightly on vibeOS booted bare metal on the x86_64 test PC, on the terms of the Phase 17 gate, reported by its §20.8 self-hosted nightly job like any other CI job

§20.1:
- [ ] the UEFI and BIOS boot paths, the firmware memory map, and the MTRR decision checked on each physical machine, its MTRR values added to the host tests
- [ ] microcode applied on the BSP and every AP of each machine, each CPU's revision before and after recorded in `docs/HARDWARE.md`
- [ ] the panic record read back after a warm reset on each machine
- [ ] a `panic_test` build that panics two CPUs at once prints one complete dump on the x86_64 test PC's own 16550 serial port at 115200 baud (F135)
- [ ] each machine's boot log names its GOP framebuffer's physical address, and the framebuffer console draws there wherever the firmware placed it (F020)
- [ ] each machine's boot log names the APIC mode its firmware handed off in (xAPIC, x2APIC, or x2APIC locked), and the machine reaches `shell ready` from that mode (F028)
- [ ] with §18.1's IOMMU on, the devices on the x86_64 test PC that DMA into its RMRR regions (integrated graphics, USB legacy emulation) keep working, and the boot log lists each RMRR with its devices

§20.2:
- [ ] idle power on real hardware at each `_CST` depth, with the governor's choices compared against measured residency
- [ ] thermal zones and fan control read through each machine's embedded controller

§20.3:
- [ ] USB serial adapters QEMU does not model: CDC-ACM as `/dev/ttyACM<N>`, and Prolific PL2303 and Silicon Labs CP210x as `/dev/ttyUSB<N>`, as Linux names them

§20.6:
- [ ] AHCI and NVMe validated on physical drives, with their error paths and SMART
- [ ] a Realtek 8168-family driver, validated on a machine that has one, and the firmware loading it and other NICs require
- [ ] CDC-NCM and the ASIX AX88179 family on a USB-C dock or adapter
- [ ] i2c sensors on each machine's own SMBus

§20.8:
- [ ] the x86_64 test PC netboots a built image nightly, driven by a self-hosted runner on the rig host, which captures its serial and switches its power, so a hung run recovers without a human; the runner's job-started hook enforces the **Self-hosted runners** rules, and a `workflow_dispatch` run on another branch fails before its first step
- [ ] cache and branch misses counted on the x86_64 test PC for a workload with a known miss pattern, and unmodified `perf stat` from the §14.9 mirror reports them
- [ ] a flamegraph from a §19.2 counter-overflow sampling profile with hardware cache-miss and branch-miss events on the x86_64 test PC, and branch-record and precise sampling through `perf_event_open` (LBR and PEBS on Intel, LBR and IBS on AMD) where its CPU reports them, its support recorded in `docs/HARDWARE.md`

Phase 21 exit gate, before the tag line:
- [ ] on the x86_64 test PC booted bare metal, with VMX or SVM enabled in firmware, a Linux kernel and vibeOS each boot to userspace as guests under vibeOS, and that vibeOS guest boots a third under its own §21.2 VMM
- [ ] on the x86_64 test PC, guests get virtio block and network devices whose sequential throughput is at least half of what the same 4-vCPU, 4 GiB guest gets under Linux KVM booted bare metal on the same machine
- [ ] vibeOS as a 2-vCPU, 2 GiB guest under Linux KVM on the x86_64 test PC, using §21.4's paravirtual clock and spinlocks, runs the §19.3 microbenchmarks within 20% of the same machine booted bare metal with 2 CPUs online (§19.6 offlining)

§21.3:
- [ ] §21.3's Linux L1 image also netboots on the x86_64 test PC as its bare-metal host when a job selects it, so each bare-metal comparison runs the same VMM on the same machine

§21.8:
- [ ] the hostile guest fuzzes 24 hours a week on the x86_64 test PC booted bare metal, with no host panic and no KASAN report, and a second guest's §19.3 microbenchmarks stay within 10% there while the first misbehaves, each guest 2 vCPUs and 2 GiB

If GitHub withdraws nested virtualization from its hosted runners, Phase 21's x86_64 nested lines,
Phase 22's x86_64 hostile-guest campaign, and the x86_64 parts of Phase 28's VF-assignment line and
Phase 30's live-update line move to this machine by the same kind of edit, with this machine's Linux KVM
as the nested host. If a Phase 24 build outgrows the hosted x86_64 runner's disk after cleanup (Phase 24 Architectures), the x86_64 half of the Phase 24 gate lines that need it moves to this machine by the same kind of edit, together with the x86_64 half of Phase 37's gate line on packages built by the Phase 24 ports tree and of Phase 39's gate line on building `v1.0.0` with the Phase 24 toolchains.

Phase 22 exit gate, before the tag line:
- [ ] the x86_64 live image, written to a USB stick, boots to its graphical desktop on the x86_64 test PC's own display through the UEFI GOP framebuffer, with a USB keyboard and mouse
- [ ] the installer installs onto the x86_64 test PC's internal NVMe or SATA disk, and the installed system boots from the firmware's boot menu and passes the §22.2 health check

Phase 22's exit gate, the line on the candidate's two 72-hour campaigns, gains: the hostile guest's
x86_64 half may run on the x86_64 test PC booted bare metal in place of the hosted shards.

§22.3:
- [ ] the tested-platforms list's physical section: one row per machine the project owns or borrows, generated from its gate records

§22.4:
- [ ] CI on vibeOS hardware: the x86_64 test PC netboots the installed vibeOS nightly and runs the §22.4 agent as the job's host; the self-hosted runner that schedules it runs on the rig host under the **Self-hosted runners** rules

§24.1:
- [ ] build times on the x86_64 test PC, booted bare metal, recorded beside the hosted KVM chains' and §17.5's

§24.2:
- [ ] the §22.4 CI agent on the x86_64 test PC, booted bare metal and driven by its self-hosted runner, runs x86_64's weekly full rebuild without shards, outside the machine's hardware-CI hours

Phase 24 exit gate, before the tag line:
- [ ] a full rebuild on the x86_64 test PC, booted bare metal, gives packages byte-identical to the hosted KVM rebuild's at the same commit, except the ports listed as not reproducing

Phase 25 exit gate, before the tag line:
- [ ] a CPU spinning with interrupts off for 10 s is reported with its backtrace by the hard lockup detector through PMU-overflow NMIs on the x86_64 test PC

§25.5:
- [ ] the hard lockup detector's NMI from PMU overflow on x86_64 (§19.2), used where CPUID leaf 0xA reports a PMU, with the buddy check kept elsewhere

§28.4:
- [ ] an assigned device's MSI-X delivered to the guest as posted interrupts where the CPU and IOMMU have them (§21.1), on the x86_64 test PC

Phase 39 exit gate, beside the nightly-configuration line:
- [ ] every physical machine that `docs/HARDWARE.md` marks as tested nightly or weekly (§39.4) passed its last run

§39.4:
- [ ] `docs/HARDWARE.md`'s physical section (§20.1) marks each physical machine as tested nightly on its self-hosted runner, tested weekly on the long-run servers, or tested by hand, with the release it was last tested at; a machine not tested at either of the last two releases leaves the list
- [ ] the release workflow refuses to cut a release while a physical nightly machine's last run is red or more than two days old
- [ ] per-machine results committed as records to §10.9's `ci-history` branch beside the QEMU runs

Beyond, two entries:
- **`rr` on vibeOS** (after 18, 19, 20, and 23): unmodified `rr` records and replays a user process on the x86_64 test PC booted bare metal, which needs Linux's `ptrace`, `perf_event_open` with the retired-conditional-branch counter over §19.2's PMU code, and seccomp-BPF; a miscompile in the self-hosted toolchain is then debugged by replay.
- **Real-time guarantees on hardware** (after 19 and the real-time latency bounds entry): bounded interrupt and scheduling latency measured on the x86_64 test PC booted bare metal, beside Linux built with `PREEMPT_RT` on the same machine, and the scheduling class's documented bound checked against it.

### Ampere Altra-class aarch64 server

**Buy.** A used Ampere Altra-class (Neoverse N1) server whose UEFI firmware is maintained and boots
Limine, with ECC memory, NVMe, an Intel NIC (igb or e1000e) onboard or in a PCIe slot, a free
CPU-attached x16 slot, and a second NVMe slot. For it to double as the aarch64 long-run server, EINJ and
GHES reporting of memory errors in its firmware, confirmed with the vendor before buying. It has neither
FEAT_NV2 nor MTE. With it: netboot, serial capture, and switched power.

**Cost.** About $3,000, and about $100 for its serial adapter and switched outlet (2026 estimates); the
rig host serves its netboot. Its lines run under the x86_64 test PC goal's rig host and self-hosted
runner.

**Unlocks.** The project's first arm64 KVM host, so aarch64 numbers kept today as HVF dev-host records
become a nightly CI job, and aarch64 microVMs, which Firecracker and cloud-hypervisor run only on a KVM
host. aarch64 on real server firmware: its ACPI tables, GIC, ITS, SPCR UART, and NVMe root, with an Intel
NIC on aarch64 silicon. SPE and BRBE sampling where the CPU has them. The aarch64 hypervisor at EL2 on
silicon, an aarch64 live image and install, and native aarch64 builds at hardware speed. A self-hosted
aarch64 runner (see **Self-hosted runners**). One machine carries every line below by schedule, the
nightly hardware-CI run first.

**Lines.** Elsewhere in this file:
- the destination paragraph: the test PC goal's "and on a real x86_64 machine" becomes "and on real machines of both architectures"
- How to read this: "and HVF on the arm64 dev host, as a §10.9 record, since hosted arm64 runners have no KVM" becomes "and, on aarch64, KVM on the aarch64 server's self-hosted runner, with HVF on the arm64 dev host as a §10.9 record"
- the arc, row 20: "bare metal on x86_64" becomes "bare metal on both architectures"
- Phase 11 exit gate, first line: "(GitHub's arm64 runners have no `/dev/kvm`, so the project has no arm64 KVM host)" becomes "and `-accel kvm` on the aarch64 server (GitHub's arm64 runners have no `/dev/kvm`)"
- §11.5, the Decided paragraph: "ACPI on aarch64 comes in §20.7, on QEMU's `virt` with ACPI and on `sbsa-ref`" gains "and on the aarch64 server's firmware"
- §11.7, the box on TCG aarch64 CI jobs, gains "; native aarch64 runs also run under KVM on the aarch64 server, beside the HVF records"
- Phase 19 Architectures: the test PC goal's "validated on the x86_64 test PC in §20.8" gains "and the aarch64 server", and the paragraph gains "aarch64 NUMA is also checked against the aarch64 server's SRAT (§20.7)"
- Phase 24 Architectures: "so aarch64's full rebuild runs monthly and x86_64's weekly (§24.2)" becomes "so x86_64's full rebuild runs weekly in hosted shards and aarch64's weekly on the aarch64 server, booted bare metal, with the hosted TCG rebuild kept monthly as a cross-check (§24.2)"

§11.7:
- [ ] a nightly aarch64 KVM leg on the aarch64 server booted into Linux, driven by a self-hosted runner on the rig host under the **Self-hosted runners** rules, that runs `make test-kernel ARCH=aarch64` under KVM and takes over, as each phase lands, the aarch64 numbers §12.3, Phase 15, Phase 16, Phase 17, §18, and Phase 19 keep as HVF dev-host records

Phase 20 exit gate, before the tag line:
- [ ] the aarch64 server boots to the shell with its root on its own NVMe, and the disk, 1 GiB fetch, and USB keyboard checks of the x86_64 test PC pass on it through its Intel NIC and xHCI
- [ ] it netboots a built image nightly under its §20.8 self-hosted nightly job, and `make test` passes on vibeOS booted on it, on the terms of the Phase 17 gate

§20.7:
- [ ] its firmware's ACPI as that firmware writes it: the FADT boot flags, MADT, GTDT, SPCR, MCFG, IORT with its RMR nodes, and SRAT, and the GIC and ITS where the firmware puts them; its `acpidump` added to the §20.2 corpus
- [ ] §20.6's igb or e1000e built and exercised on it, and serial over its own UART

§20.8:
- [ ] the aarch64 server gets the x86_64 test PC's treatment: netboot, serial captured, and power switched by the rig host, whose self-hosted runner follows the **Self-hosted runners** rules
- [ ] a flamegraph with hardware cache-miss events on the aarch64 server, and SPE and BRBE sampling through `perf_event_open` where its CPU has them, recorded in `docs/HARDWARE.md`

Phase 21 exit gate, before the tag line:
- [ ] on the aarch64 server booted bare metal at EL2, a Linux kernel and vibeOS each boot to userspace as guests under vibeOS, and their virtio block and network throughput is at least half of what the same 4-vCPU, 4 GiB guest gets under Linux KVM booted bare metal on the same machine
- [ ] vibeOS as a 2-vCPU, 2 GiB guest under Linux KVM on the aarch64 server, reading stolen time through SMCCC `PV_TIME`, runs the §19.3 microbenchmarks within 20% of the same machine booted bare metal with 2 CPUs online (§19.6 offlining)

§21.8:
- [ ] the hostile guest fuzzes 24 hours a week on the aarch64 server booted bare metal, with no host panic and no KASAN report, and the load and store differential test also runs in a vibeOS user process there

Phase 22 exit gate, before the tag line:
- [ ] the aarch64 live image, written to a USB stick, boots to its graphical desktop on the aarch64 server's UEFI GOP framebuffer (its BMC's display or a PCIe GPU), and the installer installs onto its NVMe disk, which then boots from the firmware's boot menu

§22.4:
- [ ] native aarch64 CI and release builds on vibeOS hardware: the aarch64 server netboots the installed vibeOS and runs the §22.4 agent for the aarch64 build, replacing the HVF record in Phase 22's fresh-install line

§24.1:
- [ ] build times on the aarch64 server, booted bare metal, recorded beside the hosted TCG chains' and §17.5's

§24.2:
- [ ] the §22.4 CI agent on the aarch64 server, booted bare metal and driven by its self-hosted runner, runs aarch64's weekly full rebuild and nightly changed-port rebuild without shards, with power switched and serial captured so a hung build recovers without a human

Phase 24 exit gate, before the tag line:
- [ ] a full rebuild on the aarch64 server, booted bare metal, gives packages byte-identical to the hosted TCG rebuild's at the same commit, except the ports listed as not reproducing

If a Phase 24 build outgrows the hosted arm64 runner's disk after cleanup (Phase 24 Architectures), the aarch64 half of the Phase 24 gate lines that need it moves to this machine by the same kind of edit, together with the aarch64 half of Phase 37's gate line on packages built by the Phase 24 ports tree and of Phase 39's gate line on building `v1.0.0` with the Phase 24 toolchains.

Phase 25 exit gate, before the tag line:
- [ ] `kexec` from a running system reaches `shell ready` in the new kernel without firmware in under 2 s, in a 2-vCPU, 1 GiB guest under KVM on the aarch64 server

Phase 26 exit gate, before the tag line:
- [ ] under KVM on the aarch64 server (2 vCPUs, 512 MiB), the kernel boots through a Linux arm64 `Image` header with the device tree the VMM generates, with no firmware and no Limine, to `shell ready` on Firecracker and on cloud-hypervisor, with root on virtio-blk and network on virtio-net, and on Firecracker the AWS path provisions the guest from MMDS V2

§26.4:
- [ ] a Linux arm64 `Image` header and a boot path from the device tree passed in `x0`, building `BootInfo` (§10.3) as the PVH entry does, so Firecracker and cloud-hypervisor start the kernel on aarch64 KVM hosts; entered with the MMU off, it applies the image's §18.2 relocations and builds its page tables as the PVH entry does

§26.7, Stretch:
- [ ] the aarch64 VMBus box, here or in §26.5 once the paid cloud goal has moved it, also runs under OpenVMM on KVM on the aarch64 server, as a CI job beside its dev-host record

Phase 28 exit gate, before the tag line:
- [ ] the x86_64 lines' TCP, 64-byte UDP, and accept-rate comparisons with Linux, in the same guest shape and job, on aarch64 under KVM on the aarch64 server with a multiqueue tap and `vhost-net`

Phase 29 exit gate, before the tag line:
- [ ] the `null-co` NVMe IOPS comparison with Linux, on aarch64 under KVM on the aarch64 server

Phase 30 exit gate, before the tag line:
- [ ] pgbench, nginx, and `redis-benchmark` at least 70% of Linux's throughput with 4 queue pairs on a multiqueue tap, and chrony's 1 ms discipline, on aarch64 under KVM on the aarch64 server

## Public clouds

### No-cost cloud tiers

**Open.** Accounts the maintainer opens with a card, used for nothing else. Terms as of 2026:
- AWS Free plan: accounts opened on or after 2025-07-15 get $100 in credits and can earn up to $100 more. The plan cannot bill; it ends, and the account closes, after 6 months or when the credits run out. Its EC2 shapes (`t3.micro`, `t3.small`, `t4g.micro`, `t4g.small`, `c7i-flex.large`, `m7i-flex.large`) are all Nitro, with ENA and EBS over NVMe.
- Oracle Cloud Always Free: `VM.Standard.A1.Flex` up to 2 OCPUs and 12 GB, not charged while never upgraded. An instance whose CPU, network, and memory stay under 20% for 7 days can be reclaimed.
- Google Cloud: a $300, 90-day trial, then one `e2-micro` in `us-west1`, `us-central1`, or `us-east1`, only on a paid billing account, which bills anything past the limits.
- Azure free account: $200 for 30 days, then 12 months of `B1s`, `B2pts v2` (Arm), and `B2ats v2` (AMD) only after moving to pay-as-you-go within 30 days, from which charges are possible.

Unconfirmed: whether the AWS Free plan accepts an imported image, and whether an Always-Free-only Oracle
account takes custom images.

**Cost.** $0 while use stays inside each tier.

**Unlocks.** One-time checks on real cloud platforms, which no free emulator gives: ENA and EBS's NVMe on
Nitro, on x86_64 and Graviton, within AWS's 6 months; an aarch64 VM on Ampere A1 that describes itself
with ACPI; a Google x86_64 VM on virtio-net; VMBus on Azure's Arm and AMD burstable shapes. None is a
standing gate: the AWS plan closes, Oracle reclaims idle instances, and Google and Azure can start
billing.

**Lines.** Elsewhere in this file:
- the destination paragraph: "runs production server workloads under QEMU, Firecracker, cloud-hypervisor, and OpenVMM" gains ", boots on public clouds' no-cost tiers"
- How to read this, the conditions paragraph: "and the VMM when it is not QEMU" gains "; on a public cloud, the instance type"
- the arc, row 26: Unlocks gains ", one boot on each no-cost cloud tier"
- the arc's closing paragraph: "other projects' VMMs" becomes "other projects' VMMs, public clouds"

§26.7, Stretch, the ENA driver, tested once on these instances (the paid cloud goal moves these lines into
a new **AWS: ENA** subsection before the Stretch and tests them weekly):
- [ ] admin queue, asynchronous event queue, and per-CPU submission and completion queue pairs with an MSI-X vector each
- [ ] low-latency queue mode: descriptors written into device memory through a write-combining mapping (a PAT entry on x86_64, Normal non-cacheable on aarch64), which newer Nitro shapes expect
- [ ] device reset and recovery after a missed keep-alive or a device-requested reset, without a reboot
- [ ] checksum offload through the §15.1 flags, and RSS across the per-CPU queue pairs: the driver sets a Toeplitz key and an even indirection table where the device accepts them, and keeps the device's defaults where it does not
- [ ] EBS through §20.4's NVMe driver, recognized by its PCI vendor ID and the device name in its vendor-specific Identify bytes

§26.7, Stretch, a metadata backend and one-time checks on each tier while it lasts, since each tier ends
or can bill:
- [ ] an OCI backend for the §26.1 metadata client: the platform identified by the SMBIOS chassis asset tag `OracleCloud.com`, and metadata read from the instance metadata service's v2 endpoints under `/opc/v2/` with the `Authorization: Bearer Oracle` header; the §26.1 emulator serves the same shape and asset tag, and the §26.2 service configures an image from it under QEMU on both architectures
- [ ] AWS: the release image boots on `t3.small` (x86_64) and `t4g.small` (Graviton) with root on EBS over NVMe and network on ENA, and accepts an SSH login with the key from IMDSv2
- [ ] Oracle: an aarch64 `VM.Standard.A1.Flex` instance boots the release's image, provisions itself through the OCI backend above, and accepts an SSH login, if the account takes custom images
- [ ] Google: an `e2-micro` boots the release's image with network on virtio-net and accepts an SSH login with the key from the metadata server; its boot disk needs §26.7's virtio-scsi line unless the shape offers NVMe
- [ ] Azure: `B2ats v2` (x86_64) and `B2pts v2` (aarch64) Gen2 VMs boot with root on storvsc and network on netvsc over VMBus, report ready so the deployment succeeds, and accept an SSH login; the aarch64 VM needs §26.7's aarch64 VMBus line

### Paid cloud accounts

**Open.** Pay-as-you-go accounts on AWS, Google Cloud, and Azure, used for nothing else. Before the first
launch, each gets a budget that alerts at about $35 and at about $70 runs an automated action that revokes
the CI identity's launch rights and stops every instance in the account.

**Cost.** About $100 a month with development instances and stored images, with the automated stops at
about $200 across the three (2026 estimate). None offers a hard spend cap for pay-as-you-go VMs, billing
data lags by hours, and stopped instances still pay for their disks, so spending can pass the stops; the
agents say so when asking.

**Unlocks.** Standing weekly gates on real clouds on both architectures: AWS Nitro on x86_64 and Graviton
with ENA and EBS over NVMe; Google Cloud on x86_64 and on Axion or Tau T2A with gVNIC; Azure on x86_64
and on Cobalt or Altra with VMBus and MANA. Cloud image registration and CI. Confidential guests on the
clouds that offer them. The accounts the scale-up goal rents its hosts through, at that goal's cost.

**Lines.** Elsewhere in this file:
- the destination paragraph: "runs production server workloads under QEMU, Firecracker, cloud-hypervisor, and OpenVMM", with the no-cost goal's clause if it is there, becomes "runs production servers on public clouds and under Firecracker, cloud-hypervisor, and OpenVMM"
- How to read this and the arc's closing paragraph: the no-cost goal's edits, if not already made
- the arc, row 26: Unlocks becomes "Cloud images on Firecracker, cloud-hypervisor, and OpenVMM, and public cloud instances on both architectures"

Phase 26 exit gate, before the tag line:
- [ ] Google Cloud: an x86_64 instance with virtio-net and NVMe persistent disks boots and accepts an SSH login with the key from the metadata server; it is the first cloud because it needs no new driver; the shape is G2 (`g2-standard-4`), the one series with both, whose GPU goes unused but needs quota in the project
- [ ] Google Cloud: x86_64 and aarch64 instances boot with gVNIC and NVMe persistent disks, with keys from the metadata server
- [ ] AWS: x86_64 Nitro and Graviton instances boot with root on EBS over NVMe and network on ENA, and accept an SSH login with the key from IMDSv2
- [ ] Azure: x86_64 and aarch64 Gen2 VMs boot with root on storvsc and network on netvsc over VMBus, report ready so the deployment succeeds, and accept an SSH login
- [ ] a volume attached to a running instance on each cloud appears under a persistent name, carrying the volume id on AWS, and detaches cleanly
- [ ] a deliberate panic on each cloud reboots the instance under §22.2's panic policy, and the panic text is in the console output the CI job fetches
- [ ] a weekly job launches the current image on each cloud and architecture, asserts the DESIGN §8.3 markers from the console output, runs the SSH smoke test, terminates every instance, and records the cost of the run

Phase 26, a new subsection before the Stretch, **AWS: ENA**, holding the no-cost goal's ENA lines, taken
out of §26.7 if that goal put them there; and a new subsection before the Stretch, **Google Cloud:
gVNIC**:
- [ ] admin queue and both queue formats, GQI with registered queue page lists and DQO with raw addressing, since the machine type picks one
- [ ] MTU from the device, up to the 8896 bytes Google's networks carry, and receive spread across the queues by the device's RSS, with the key and indirection table set by the driver where the device offers RSS configuration

§26.3:
- [ ] Azure volume attach and detach through storvsc LUN rescans: LUNs added and removed when the host signals a bus change, with I/O to a removed LUN failed as §20.9's removal does

§26.5, which also takes §26.7's aarch64 VMBus box, since the Azure line needs it:
- [ ] the Hyper-V hypercall interface and SynIC through hypercalls on aarch64, on Azure's aarch64 shapes
- [ ] the heartbeat, shutdown, and time sync integration services, so the portal's stop and restart work
- [ ] netvsc with accelerated networking, MANA as its virtual function

§26.6:
- [ ] per-cloud registration scripts (an AMI with UEFI boot and ENA support set, a Google image, an Azure gallery image), idempotent and run by the release job
- [ ] CI authenticates to each cloud through OIDC federation, so no long-lived cloud credential is stored with the repository; each cloud trusts only the subject of one GitHub environment, `cloud`, which only the weekly cloud workflow and the release workflow's registration job use and whose deployment policy admits only `main` and `gate/*` branches, those branches created only by the maintainer under a repository ruleset; `release.yml` runs from `main` (§10.1), and its registration job takes the images from the `build` job and runs `main`'s registration scripts, never the release tag's; §10.9's dispatch of the weekly job at a gated commit runs on a `gate/*` branch, and a run from any other ref, a pull request's included, cannot take the launch role
- [ ] every instance the job starts is tagged with the run id and a deadline, and a sweeper deletes anything past its deadline, so a crashed job cannot leave instances billing
- [ ] the harness gains a cloud backend: console output fetched from the cloud's API and asserted against the same marker contract

§26.7, Stretch:
- [ ] UEFI Secure Boot and the virtual TPM on each cloud, with §18.7's signed chain and measured boot
- [ ] Azure's NVMe remote disks
- [ ] IPv6-only instances, with metadata over IPv6 where the platform serves it
- [ ] a KVM-based OpenStack cloud in the weekly job

Beyond:
- **Confidential guests** (after 26 and 27): vibeOS as an AMD SEV-SNP, Intel TDX, and Arm CCA guest on the clouds that offer them, with private memory, Phase 27's bounce pool for shared I/O, and a remote attestation report checked by a host tool; the host is untrusted there, so the virtio transport relies on §18.1's checks of the used-ring ids, its queue-size cap on each harvest pass, and the capability offsets and lengths a device supplies (F048).

### Scale-up on rented bare-metal hosts

**Rent.** A Linux KVM host of each architecture with at least 64 physical cores and 256 GiB, rented by
the hour as a bare-metal cloud instance, two-socket for the NUMA lines, with vibeOS and Linux as guests on
it. Needs the paid cloud accounts.

**Cost.** About $3 to $8 an hour, about $1,000 over Phase 27 (2026 estimate).

**Unlocks.** Scaling measured on real cores against Linux, which 4-vCPU runners under TCG cannot show:
AP bring-up time, a 1 TiB guest's boot time, and speedup from 1 to 64 vCPUs. §19.7's NUMA code against
real remote-memory latency, which QEMU's `-numa` does not model.

**Lines.** Elsewhere in this file:
- the arc, row 27: Unlocks gains ", speedups on 64 real cores"

§19.7:
- [ ] in a KVM guest whose two virtual nodes are pinned to the two sockets of the rented x86_64 host, node-local allocation and NUMA-aware scheduling cut remote-node accesses and the run time of a memory-bound §19.3 workload against an interleaved baseline, the numbers recorded in `docs/`

§20.7:
- [ ] the same on the rented aarch64 host, in a KVM guest booted through ACPI on `virt` under the aarch64 edk2 build, so its nodes come from SRAT

Phase 27 exit gate, before the tag line:
- [ ] AP bring-up from the first SIPI or `CPU_ON` to `smp: done` takes under 500 ms in a 64-vCPU, 8 GiB guest under KVM on the rented host of each architecture, and the time is on the boot line
- [ ] a 1 TiB guest with sparse host backing boots under KVM on the rented host of each architecture no more than 5 s slower than a 4 GiB guest with the same 8 vCPUs, and `meminfo` reports the full total
- [ ] in a 64-vCPU, 32 GiB guest under KVM on the rented host of each architecture, private page faults, `open`/`close` of per-thread files, `pipe` ping-pong pairs, and per-thread `mmap`/`munmap`, each run as 64 processes and as 64 threads of one process, reach at least 90% of the speedup from 1 to 64 that Linux reaches in the same guest, with both sets of numbers recorded
- [ ] the §17.5 self-build with 64 jobs in that guest gets at least 70% of the speedup over one job that Linux gets in the same guest

§27.5:
- [ ] the queued `SpinMutex` measured at 64 CPUs on the rented hosts against a build of the commit before §27.5 replaced the compare-and-swap lock, and the load balancer's cost per tick measured there

## Hosted CI capacity

### GitHub Pro

**Open.** GitHub Pro on the maintainer's account, which owns the repository.

**Cost.** About $4 a month (2026 price), to confirm before buying. Public-repository minutes stay free.

**Unlocks.** 40 concurrent hosted jobs instead of 20, 5 of them macOS either way, so the Phase 24
rebuilds take 10 jobs without starving per-push CI and the other scheduled campaigns, which roughly halves
the wall time of aarch64's TCG full rebuild. Pro covers only a personal account, so it stops applying if
the repository moves into an organization (see **GitHub GPU runner**).

**Lines.**
- How to read this, the list of free resources: "at most 20 jobs run at once, 5 of them macOS" becomes "at most 40 jobs run at once with GitHub Pro, 5 of them macOS"
- §24.2, the full-rebuild box: "sum to at most 5, their share of §10.1's 10 scheduled slots, so a rebuild that runs for days leaves the other 5 to the nightly and weekly workflows and per-push CI its own 10" becomes "sum to at most 10, their share of §10.1's 20 scheduled slots (GitHub Pro), so a rebuild that runs for days leaves the other 10 to the nightly and weekly workflows and per-push CI its own 20"
- §10.1, the CI-budget box: "20 concurrent jobs on the Free plan (at most 5 macOS; scheduled campaigns together hold at most 10, so pushes keep the other 10)" becomes "40 concurrent jobs with GitHub Pro (at most 5 macOS; scheduled campaigns together hold at most 20, so pushes keep the other 20)"; "leave room under the 20" becomes "leave room under the 40"; "its share of the 10 scheduled slots" becomes "its share of the 20 scheduled slots"; and "shares summing to at most 10" becomes "shares summing to at most 20"
- §24.2's "monthly on aarch64" and Phase 24 Architectures' "aarch64's full rebuild runs monthly": "monthly" becomes "every two weeks, once a measured aarch64 full rebuild at 10 jobs finishes in under ten days", unless the aarch64 server goal has already moved aarch64's full rebuild to that server
- §20.8's preamble: "an account runs 20 jobs at once" becomes "the account runs 40 jobs at once (GitHub Pro)"
- §39.3, the supported-branches box: "§10.1's scheduled share of 10 concurrent jobs (at most 5 of them macOS)" becomes "§10.1's scheduled share of 20 concurrent jobs (at most 5 of them macOS)", and "so pushes keep the other 10" becomes "so pushes keep the other 20"

### GitHub GPU runner

**Open.** A GitHub Team organization, with the repository transferred into it from the maintainer's
account, and a GPU larger runner: Linux, 4 vCPUs, one Tesla T4, 28 GB of RAM, 16 GB of VRAM.

**Cost.** $0.052 a minute, about $3.12 an hour of GPU run, billed even for public repositories, plus
GitHub Team at about $4 a member a month (2026 prices), to confirm before buying.

**Unlocks.** virgl and Venus in CI with the host rendering on a GPU instead of `llvmpipe`, so frame rates
reflect a GPU-backed host. The organization's plan replaces the maintainer's account's: 60 concurrent
hosted jobs, 5 of them macOS.

**Lines.** Elsewhere in this file:
- the arc, row 33: Unlocks gains ", virgl and Venus on a GPU host"
- the **GitHub Pro** goal, if still open, is deleted, since Pro covers only a personal account; its Lines are made, or where already made are changed, with GitHub Team's numbers: 60 concurrent jobs for 40; §10.1's scheduled share 40 for 20, so pushes keep 20; the §24.2 rebuilds' share, the rest of the scheduled share that §24.2 leaves to the other workflows, and the job count in the aarch64 cadence, 20 for 10; "GitHub Pro" becomes "GitHub Team"

§33.4, Stretch:
- [ ] the virtio-gpu 3D tier also runs nightly on a GitHub GPU runner with the host rendering on its GPU, its frame rates recorded

## Long runs and scale

### Data-center NVMe drives

**Buy.** One data-center NVMe drive of one model for each test machine, in its second NVMe slot,
separate from its root disk.

**Cost.** About $300 each, $600 for both (2026 estimate). Needs both test machines.

**Unlocks.** IOPS against Linux on a real data-center drive, which QEMU's model and the `null-co`
comparison cannot show, and writeback measured on real flash.

**Lines.** Elsewhere in this file:
- the arc, row 29: Unlocks gains ", IOPS on real drives"

Phase 29 exit gate, before the tag line:
- [ ] 4 KiB random reads on the data-center drive in each test machine reach at least 90% of Linux's IOPS on the same machine, with `fio` from the §14.9 mirror on both kernels, using the `io_uring` engine with `direct=1` (§19.8), at the same queue depth and job count, so the gate never measures a mounted root

§29.5:
- [ ] the parallel writeback measurements repeated on the data-center drive in each test machine

### Long-run servers

**Buy.** A long-run server of each architecture, so soaks do not take the test machines from their
nightly runs. x86_64: ECC memory; EINJ, ERST, and firmware-first (GHES) reporting of corrected memory
errors in its firmware; a serial port; a BMC. aarch64: an Ampere Altra-class server with ECC memory and
EINJ and GHES reporting of memory errors in its firmware. Each confirmed with the vendor before buying.
With them: netboot, serial capture, switched power, and a Linux peer for the soaks' TCP. A test machine
whose firmware qualifies can serve instead, at the cost of its nightly hours.

**Cost.** About $1,200 used for the x86_64 server and about $3,000 for the aarch64 one (2026 estimate).
Needs the x86_64 test PC's rig host, which drives them.

**Unlocks.** Errors injected and reported through real firmware (EINJ, ERST, GHES, CMCI) on real ECC
memory. Continuous 72-hour soaks and 30-day uptime, unsharded. A real BMC. Live update on bare metal.
Physical release soaks for 1.0 and after. With the x86_64 test PC and a direct Ethernet cable, live
migration between machines. Soaks run under the same self-hosted runner rules (see **Self-hosted
runners**).

**Lines.** Elsewhere in this file:
- How to read this, the long-runs paragraph: "uptime counted in weeks is a funded goal" becomes "uptime counted in weeks runs on the long-run servers (§25.7)"
- the arc, row 25: Unlocks becomes "Machine checks on real ECC memory, crash dumps, watchdogs, persistent logs"
- the arc, row 30: Unlocks becomes "Server software unattended for a month, metrics, live update"

Phase 25 exit gate, before the tag line:
- [ ] on the x86_64 long-run server, a corrected memory error injected through ACPI EINJ arrives as a decoded CPER record naming the DIMM from SMBIOS; the aarch64 long-run server passes the same test through GHES, and an uncorrected error it injects through EINJ into a user page sends `SIGBUS` to the process that reads the page
- [ ] after a panic and a cold restart, the next boot logs the panic record from the firmware's ERST on the x86_64 long-run server
- [ ] a 72-hour continuous soak on the long-run server of each architecture (fork and exec, file I/O with `fsync`, TCP to its Linux peer) ends with no panic, and §25.7's slope check projects under 1% growth over 30 days for frame, heap, slab, and descriptor counts

§25.1:
- [ ] corrected errors through CMCI with a per-bank threshold, the poll timer kept where CMCI is absent, tested with EINJ on the x86_64 long-run server

§25.2:
- [ ] GHES sources notified by SCI or NMI on x86_64, and by SEA, SEI, SDEI, or a GPIO controller's `_AEI` pin (its `_EVT`, `_Exx`, or `_Lxx` method run on §20.2's interpreter) on aarch64, as the long-run servers' firmware signals them
- [ ] SError, synchronous external aborts from real memory errors, and the RAS extension's error records where firmware leaves them to the OS, on the aarch64 long-run server
- [ ] an in-guest EINJ test on machines whose firmware has the table, `ktest_skip`ped with the reason elsewhere

§25.7:
- [ ] the long-run servers in `docs/HARDWARE.md`'s physical section, under the rig host's netboot, serial capture, power control, and self-hosted runner, with a Linux peer for the soaks' TCP

§25.8, Stretch:
- [ ] IPMI over KCS or SSIF on the x86_64 long-run server's BMC: panics and machine checks written to its system event log, and the BMC watchdog as a second watchdog

Phase 30 exit gate, before the tag line:
- [ ] 30 days of uptime on the long-run server of each architecture running the §30.1 services and the §17.5 build loop: no panic, no watchdog reset, kernel memory and descriptor counts within 2% of day one, and the wall clock within 10 ms of an NTP server at every hourly sample
- [ ] a host kernel is replaced through kexec while a 2-vCPU, 2 GiB §21.2 guest keeps its memory in place, and a ping loop from the guest to the Linux peer misses at most 2 s, on the long-run server of each architecture

§30.7:
- [ ] the 30-day run repeated each quarter on the long-run servers, in place of that month's 7-day run on the §25.7 soak job

Phase 39 exit gate, beside the hosted 168-hour line:
- [ ] the release candidate runs §30.7's long-run workload for 7 days without a reboot on the long-run server of each architecture, starting after any 30-day run in progress ends, and records no panic, no Phase 25 watchdog reset, and no uncorrected machine check

§39.3, beside the hosted 72-hour line, and in its crash-record line "nightly or soak" becomes "nightly, soak, or long run":
- [ ] each later release candidate, and each patch release on a supported branch, also passes a 72-hour soak of §30.7's workload on the long-run servers on the commit being released, before it is cut. A release candidate due while a 30-day run holds those servers waits for the run to end; a patch release stops the run, which is recorded as neither green nor red, soaks each supported branch's commit in turn, and restarts the 30-day run from day one. Each soak is a `workflow_dispatch` run on `main` that takes the commit as input and names it in its §10.9 CI-history record, which the release workflow checks

Beyond:
- **Live migration between machines** (after 21, 28, and the live migration entry): a running §21.2 guest moved over TCP from the x86_64 test PC to the x86_64 long-run server, with a CPU feature set both machines offer, dirty pages tracked through EPT or NPT dirty logging and pre-copied, and the downtime of a 4-vCPU, 4 GiB guest running §19.3's mixed interactive workload measured and held under 300 ms; the same between two aarch64 machines with stage-2 dirty logging once both exist.

### 100GbE NICs

**Buy.** Three dual-port 100GbE NICs of one model, NVIDIA ConnectX-6 Dx or Intel E810, two
direct-attach cables, and a Linux peer machine with a PCIe 4.0 x16 slot. Each test machine takes one NIC
in a free CPU-attached x16 slot.

**Cost.** About $4,100: about $800 per NIC, about $100 per cable, and about $1,500 for the peer (2026
estimate). Needs both test machines.

**Unlocks.** Line rate and packet rate on a physical NIC against Linux on the same machine, an SR-IOV VF
at near the physical function's rate, hardware TSO, LRO, and PTP, and RoCEv2 on hardware.

**Lines.** Elsewhere in this file:
- the arc, row 28: Unlocks gains ", 100GbE at line rate"

Phase 28 exit gate, before the tag line:
- [ ] TCP between each test machine and the peer, over 8 streams in each direction, reaches at least 80 Gbit/s and at least 80% of Linux's rate on the same machine, NIC, cable, and peer
- [ ] the 64-byte UDP receive rate on each test machine is at least 50% of Linux's on the same machine, NIC, and peer
- [ ] the RSS placement test of 64 UDP flows on the 100GbE NIC with 8 queues set by `ethtool -L` on each test machine
- [ ] virtio-net with 4 queue pairs and vhost on the host reaches at least 2.5 times its single-queue TCP throughput, in a 4-vCPU, 4 GiB guest under KVM on each test machine
- [ ] an SR-IOV virtual function of the 100GbE NIC assigned to a 4-vCPU, 4 GiB §21.2 guest through the §18.1 IOMMU carries TCP to the peer over 8 streams in each direction at no less than 90% of the rate vibeOS reaches over the same streams on the physical function, booted bare metal on the same machine with 4 CPUs online (§19.6 offlining)

Phase 28, a new subsection before the Stretch, **100GbE driver**:
- [ ] one 100GbE PCIe NIC driver for the model bought, NVIDIA ConnectX (mlx5) or Intel E810 (`ice`): firmware command interface, queues, RSS, and offloads, with its BARs mapped through §20.1's sized `ioremap` window
- [ ] its virtual function driver as well, which §28.4 assigns
- [ ] link state, speed, and FEC mode reported; §25.2's `error_detected` hook implemented
- [ ] the peer's dual-port NIC cabled to each test machine, and one script that takes the vibeOS and Linux measurements back to back

§28.2:
- [ ] TCP segmentation offload on the 100GbE NIC
- [ ] hardware LRO where the device does it correctly

§28.6, Stretch:
- [ ] hardware timestamping and a PTP hardware clock on the 100GbE NIC

Beyond:
- **RDMA on hardware** (after 28 and the software RDMA entry): RoCEv2 on the 100GbE NIC through Linux's verbs ABI, so unmodified `rdma-core` runs, then NVMe over RDMA; kernel-bypass user queues isolated by the §18.1 IOMMU.

## Daily-driver hardware

These goals take Era VII from a VM to the laptop and desktop someone uses every day, each number measured
against a pinned Fedora Workstation on the same machine. Each needs the x86_64 test PC's rig host, which
runs the rig. The first adds what the rest share: the rig and the reference-machines paragraph. The
reference laptop's Phase 35 to 37 lines also need the wireless rig's access points, and wait for them.

### Reference laptop

**Buy.** A Framework Laptop 13 with the newest Intel Core Ultra that Linux's `xe` driver supports by
default and an Intel AX210 in its M.2 slot. For the rig: a microcontroller HID injector each for the
laptop and the aarch64 server (USB keyboard, mouse, and precision touchpad), a lid magnet and a
power-button actuator, a switched outlet for the charger, and a USB 3 debug cable.

**Cost.** About $1,600 for the laptop and about $200 for the rig parts (2026 estimates).

**Unlocks.** The laptop half of Era VII, against Fedora on the same machine: the embedded controller,
battery, lid, ACPI and WMI hotkeys, and an I2C-HID touchpad; s2idle with S0ix residency and suspended
drain; native Intel display (panel, backlight, PSR, DisplayPort link code); `xe` rendering with VA-API
decode; laptop audio, with codec quirks, jack sensing, and SOF microphones; a UVC webcam; and the AX210
driver for Wi-Fi and Bluetooth. The external-monitor, access-point, and audio-interface lines also need
the goals after this one.

**Lines.** Elsewhere in this file:
- the destination paragraph: "becomes a desktop someone could use every day in a VM" gains "and the laptop someone uses every day"
- the arc, rows 31 to 37: Unlocks gain the reference laptop's part: 31 ", a laptop's battery, lid, and touchpad"; 32 ", native Intel display"; 33 ", `xe` and hardware video decode"; 34 ", laptop audio and a webcam"; 35 ", the AX210's Wi-Fi and Bluetooth"; 37 ", on the reference laptop against Fedora"
- the Era VII preamble, after **Hosts.**, a **Reference machines** paragraph: a line that names the reference laptop, the desktop, or the aarch64 server holds on that machine in the rig, and compares with a pinned Fedora Workstation image the rig boots on the same machine: the component built with Fedora's packaging from the upstream release vibeOS runs, both version strings recorded, and a version mismatch reported as a failed comparison. The reference laptop proves Intel display, GPU, Wi-Fi, and Bluetooth, an I2C-HID touchpad, a UVC camera, Intel audio, and s2idle with no S3

Phase 31 exit gate, before the tag line:
- [ ] 500 consecutive s2idle cycles on the reference laptop, woken alternately by the RTC alarm and by a keypress from the rig's HID injector, with no hang, the same device list after every resume, and a 1 GiB fetch from the rig host over a USB Ethernet adapter (§20.6) with no corruption after the last cycle
- [ ] S0ix residency above 90% over a 10-minute suspend on the reference laptop, from the PMC's `SLP_S0` residency counter
- [ ] suspended battery drain over 8 hours on the reference laptop at most 1.5 times Fedora's on the same machine, from the battery's own charge readings
- [ ] on the reference laptop, the rig's lid magnet suspends it and releasing the lid resumes it; the power-button actuator starts an orderly shutdown through init; switching the charger's outlet off and on is reported within 2 s by the kernel and by UPower from Alpine
- [ ] `libinput list-devices` from Alpine reports the reference laptop's touchpad, keyboard, and lid switch with the capabilities Fedora reports on the same machine
- [ ] the rig's HID injector, presenting as a USB precision touchpad, drives tap, two-finger scroll, pinch, and two-finger right click through libinput on the reference laptop, checked from `libinput debug-events`
- [ ] every hotkey in a list captured with `evtest` under Fedora on the reference laptop and checked in produces the same evdev key in a host test: scan codes through the §5.2 decoder, and ACPI and WMI events through the §20.2 interpreter on the machine's `acpidump` tables
- [ ] on the reference laptop, PCIe links reach their L1 substates and idle devices D3cold wherever Fedora reaches them on the same machine (from `lspci -vv` and `power/runtime_status` captured under Fedora and checked in), each resumes on use, and the §31.4 counters show it
- [ ] the loader serves §20.1's microcode on the reference laptop

§31.1, host-tested against the linuxhw/ACPI corpus's notebook tables before the laptop arrives:
- [ ] the ACPI embedded controller: `ECDT` for early access, the EC address-space handler in the §20.2 interpreter, `_Qxx` query events from its GPE, and burst mode; the battery and AC lines, the lid switch, and the laptop's §20.2 thermal zones read through it
- [ ] Intel's GPIO pin controllers with `GpioInt` and `GpioIo` resources, since the touchpad's interrupt is a GPIO line
- [ ] Synopsys DesignWare I2C controllers (Intel LPSS) enumerated from `I2cSerialBusV2` resources, extending §20.6's i2c line past SMBus
- [ ] hotkeys that arrive through WMI (`PNP0C14`), `_DSM`, or the ACPI video device's notifications rather than as scan codes
- [ ] UCSI (`PNP0CA0`): each USB-C port's role, partner, and power contract in Linux's `/sys/class/typec` layout; DP alt mode is the **Intel display** subsection's
- [ ] a debug option that writes a hash of each device's name into the RTC before its suspend or resume hook runs, as Linux's `pm_trace` does, and decodes it on the next boot, since the forced power-off that recovers a hung laptop clears §20.1's RAM record; off by default because it overwrites the wall clock

§31.2:
- [ ] I2C-HID: descriptor fetch, input reports on the GPIO interrupt, reset and power commands, sharing §20.3's report parser
- [ ] the lid as `SW_LID`, and the §31.1 ACPI and WMI hotkeys as their named keys (`KEY_BRIGHTNESSUP`, `KEY_RFKILL`, and the rest)
- [ ] `libinput record` captures of the reference laptop's touchpad, taken on Fedora, checked in and replayed through `uinput` in CI on both architectures

§31.3:
- [ ] the LPS0 `_DSM` calls around the idle, the PMC's residency read back after resume, and the device that blocked S0ix named when residency is low

§31.4:
- [ ] PCIe ASPM with L1 substates from the link capabilities, honoring `_OSC` and the FADT's ASPM bit
- [ ] D3cold through `_PR0` and `_PR3` power resources
- [ ] NVMe autonomous power state transitions

§31.5:
- [ ] the battery and AC adapter in Linux's `power_supply` class: status, charge, design and full capacity, cycle count, and rate, with a uevent on each change, so UPower from Alpine reads them unmodified
- [ ] a charge limit as `charge_control_end_threshold`, where the EC exposes one
- [ ] §20.2's frequency governor in Linux's `cpufreq` sysfs layout with the energy-performance preference, and `/sys/firmware/acpi/platform_profile` where the firmware has profiles, so power-profiles-daemon from Alpine switches profiles unmodified
- [ ] suspend on lid close and on low battery, and hibernation (§31.8) on critical battery (an orderly shutdown where hibernation is unavailable), as a service under §14.3's init, logged either way

§31.7:
- [ ] the reference laptop in a rig: switched power, with its charger on its own outlet for battery runs, and serial over the xHCI debug capability through USB-C, read by the rig host as a serial port
- [ ] a microcontroller HID injector presenting as a USB keyboard, mouse, and precision touchpad, and a lid magnet and power-button actuator, driven by the harness
- [ ] a pinned Fedora Workstation image the rig boots on the laptop; each comparison script runs unchanged on vibeOS and Fedora and records both results in one format, per release

§31.8, Stretch:
- [ ] S3 on a laptop whose firmware still offers it, sharing §31.3's device ordering

Phase 32 exit gate, before the tag line:
- [ ] the reference laptop's panel runs at its native mode through the native driver
- [ ] IGT's KMS tests on a checked-in list pass on the reference laptop, failing none that passes on Fedora on the same machine
- [ ] the laptop panel's brightness takes at least 16 levels through `/sys/class/backlight`, and its level is restored after resume
- [ ] every output on the reference laptop returns with its mode, layout, and content after 100 suspend cycles
- [ ] a page flip on every vblank for 10 minutes at each output's refresh rate on the reference laptop, with fewer than 0.1% missed, from the vblank counter
- [ ] a mode the link cannot carry, forced by capping the DisplayPort link rate, falls back to a lower mode instead of a black screen
- [ ] idle power at the shell on the reference laptop, the panel in self refresh at the `/sys/class/backlight` level Fedora's run used, within 20% of Fedora's, from the battery's reported power draw with the charger's outlet off, averaged over 10 minutes
- [ ] the §16.3 compositor runs on the native driver, using overlay and cursor planes when the atomic check accepts them

Phase 32, a new subsection before the Stretch, **Display links**, host-tested before the laptop arrives:
- [ ] DisplayPort: AUX and DPCD, link training through clock recovery and channel equalization, and link-rate and lane-count fallback; the training state machine host-tested
- [ ] DP MST: sideband messages, topology discovery, and payload allocation; host-tested against captured sideband traffic
- [ ] Display Stream Compression: the sink's DSC capabilities from the DPCD, the picture parameter set, and slice configuration, used when a mode exceeds the link's bandwidth, over MST too; the parameter computation host-tested
- [ ] HDMI 2.0: SCDC scrambling for 4K 60 Hz, AVI and audio infoframes, and the ELD that §34.2 reads
- [ ] hotplug and DisplayPort short-pulse interrupts, and hardware vblank interrupts, behind §32.1's connector changes and counters

Phase 32, a new subsection before the Stretch, **Intel display**:
- [ ] power wells and the DMC firmware for display power states, loaded through §31.6
- [ ] pipes, planes including the cursor, transcoders, DDI ports, PLLs, and watermarks, from Intel's published graphics documentation for the reference laptop's generation
- [ ] the VBT from the ACPI OpRegion: ports, panel, and backlight controller
- [ ] eDP: panel power sequencing from the VBT, and PSR, or Panel Replay where the panel has it, at idle
- [ ] backlight through the PWM or the DPCD AUX interface, whichever the VBT names, as Linux's `/sys/class/backlight`
- [ ] framebuffer compression on the primary plane, measured against idle power without it
- [ ] Type-C ports: DP alt mode entry from §31.1's UCSI state, and the Type-C PHY ownership handshake
- [ ] the DSC engines, for the **Display links** compressed modes
- [ ] pipe CRCs from the display engine, in §32.1's debugfs layout

Phase 33 exit gate, before the tag line:
- [ ] on the reference laptop, dEQP through `deqp-runner`, with the suites and fractions of Mesa's CI configs for the machine's drivers at the release under test (`src/intel/ci` for `iris` and `anv`, with `renderer_check` set to the machine's GPU) and one checked-in skip list for both runs whose entries each name a reason, passes within 2 percentage points of the same Mesa release on Fedora on the same machine, with the differing tests listed
- [ ] IGT's `xe` tests on a checked-in list pass on the reference laptop, failing none that passes on Fedora on the same machine
- [ ] `glmark2-es2-drm` on the reference laptop scores at least 70% of Fedora's on the same machine with the same Mesa release
- [ ] on the reference laptop, a shader that never terminates is detected, its context banned, and the engine reset, while another client keeps rendering
- [ ] a GPU client killed mid-frame leaves no buffer, mapping, or GPU address space behind, from the card's debugfs counts
- [ ] 4K clips in each codec the reference laptop's hardware decodes (H.264, HEVC, VP9, AV1) decode through VA-API with `ffmpeg` from Alpine at 60 frames per second or better, every frame's checksum matching `ffmpeg`'s software decode
- [ ] with the hardware render driver disabled on the reference laptop, `llvmpipe` from Alpine renders `kmscube` on the native display

§33.1:
- [ ] a GPU scheduler: per-context queues, fence dependencies, job timeouts, and engine reset

Phase 33, a new subsection before the Stretch, **Intel GPU**:
- [ ] the `xe` uapi, which Mesa's `iris` and `anv` speak: buffer creation, `VM_BIND` into per-process GPU address spaces, exec queues, and syncobjs; i915's uapi only if `xe` does not support the reference GPU, decided before any code and written down
- [ ] render, copy, compute, and video engines, with GuC submission and HuC, their firmware through §31.6
- [ ] GPU page tables per address space, and TLB invalidation through the GuC
- [ ] engine and GT reset, with the guilty context banned and the others resubmitted
- [ ] GT power: RC6, and frequency through the GuC's SLPC, under §31.4's runtime PM
- [ ] VA-API through Intel's media driver from Alpine, and Vulkan Video in `anv` as the second path

Phase 34 exit gate, before the tag line:
- [ ] the reference laptop's built-in microphone records its own speakers playing the tone, with auto-mute turned off through its ALSA control, and `alsabat` finds the peak
- [ ] the reference laptop's camera streams 1080p at 30 frames per second for 10 minutes with under 1% of frames dropped, from the V4L2 sequence numbers; `v4l2-compliance` fails nothing on it that it passes on Fedora on the same machine

§34.2:
- [ ] codec dumps from the reference laptop as host tests of the widget-graph parser
- [ ] a quirk table keyed by codec and subsystem id, since laptop pin defaults are routinely wrong
- [ ] jack detection through unsolicited responses into §34.1's jack controls and evdev's `SW_HEADPHONE_INSERT`
- [ ] the reference laptop's microphones on the path Linux uses on that machine, read from Fedora's boot log before any code: the HDA codec, or Intel's SOF DSP with its signed firmware and topology through §31.6

§34.5:
- [ ] UVC: control and streaming interfaces, isochronous and bulk streaming, MJPEG and YUYV

§35.1:
- [ ] soft-MAC devices with firmware offload, such as the AX210, where the firmware does rate control; power save through the firmware's offloads

Phase 35, a new subsection before the Stretch, **Intel AX210**:
- [ ] the PCIe transport, firmware through §31.6, and the operation-mode command interface
- [ ] suspend and resume hooks, so §31.3's cycles leave the radio usable

§35.5:
- [ ] HCI over USB, and the AX210's controller firmware and patch download through §31.6

Phase 36 exit gate, before the tag line:
- [ ] the §36.2 desktop starts from its display manager on the reference laptop
- [ ] on the reference laptop, the session locks on lid close, shown by logind's `LockedHint`
- [ ] the §36.2 desktop composes on the GPU on the reference laptop: with ten windows moving at the panel's native resolution, the compositor's CPU time stays under 10% of one core
- [ ] `mpv` plays a 4K 60 Hz clip in each codec the Phase 33 decode line names, through VA-API and PipeWire, with under 1% of frames dropped and under 15% of one core, on the reference laptop

§36.4:
- [ ] GPU compositing and WebGL through the native render driver, and video through VA-API

Phase 37 exit gate, before the tag line:
- [ ] the §22.2 installer puts the desktop on the reference laptop's internal disk beside an existing Fedora install; both boot afterwards
- [ ] the §37.1 workload runs for 24 hours on the reference laptop with no kernel panic, no hang needing a power cycle, no data loss, and no crash outside the injected ones
- [ ] the 8-hour §37.1 nightly run on the rig has passed on the reference laptop on 30 consecutive nights, from §10.9's run history
- [ ] 1000 consecutive s2idle cycles on the reference laptop, with Wi-Fi, Bluetooth, audio, and every output working after the last
- [ ] battery life on the reference laptop, running §37.1's browsing and video loop from full to 5% at Fedora's backlight level, at least 80% of Fedora's on the same machine
- [ ] boot to the display manager, resume to the lock screen, and a build of the vibeOS tree each take at most 1.5 times Fedora's time on the reference laptop
- [ ] the §22.3 daily-driver tier gains the reference laptop with its numbers

§37.1:
- [ ] the workload's browsing and local-video steps also run alone as a loop, which the battery-life line runs; an 8-hour run nightly on the rig, with a record per machine to §10.9's `ci-history` branch

§37.3:
- [ ] battery life and idle and suspended power for the reference laptop, and for Fedora on it, in the release results file

§37.5, Stretch:
- [ ] a person uses the reference laptop as their only computer for 14 consecutive days; its session log shows the days, and every problem filed has a regression test or an open box in this file

### Display peripherals

**Buy.** Two 4K 60 Hz DisplayPort monitors, a USB-C dock with DP MST and a USB Ethernet chip §20.6
drives, HDMI capture with DisplayPort adapters, and a CI-controlled video switch.

**Cost.** About $1,300 (2026 estimate). Needs the reference laptop.

**Unlocks.** External monitors on the native display driver: hotplug through a real video switch, a dock
with DP MST, and reference images captured from real outputs.

**Lines.** Elsewhere in this file:
- the arc, row 32: Unlocks gains ", external monitors and docks"

Phase 32 exit gate, before the tag line:
- [ ] a 4K 60 Hz monitor on the reference laptop's USB-C port, disconnected and reconnected 100 times through the rig's video switch, gets its mode and layout back each time, and the card's framebuffer and buffer-object counts return to their starting values
- [ ] the dock drives two 4K 60 Hz monitors through DP MST from the reference laptop
- [ ] the §16.1 reference-image comparison passes on each reference-laptop output that the rig's HDMI capture device sees

§31.7:
- [ ] HDMI capture with DisplayPort adapters and a CI-controlled video switch, driven by the harness

§32.5, Stretch:
- [ ] variable refresh rate, and HDR with 10-bit output, on a monitor that has them

Phase 36 exit gate, before the tag line:
- [ ] after a suspend, the first frame the rig captures from the reference laptop's external output on resume is the lock screen

§37.1:
- [ ] the scripted day docks and undocks through the rig's video switch

### Wireless rig

**Buy.** Two Wi-Fi 6E access points with WPA3 that the rig controls, AX210 cards for the desktop and for
the aarch64 server on a PCIe adapter, and microcontrollers for a BLE keyboard and mouse and a Classic
Bluetooth A2DP sink. An Intel BE200 for the Wi-Fi 7 stretch.

**Cost.** About $450, and about $30 for the BE200 (2026 estimates). Needs the reference laptop.

**Unlocks.** Real radios on real air: association on every band, throughput at distance against Fedora,
suspend and power save with Wi-Fi up, and BLE and A2DP pairing with real peripherals.

**Lines.** Phase 35 exit gate, before the tag line:
- [ ] the reference laptop joins the rig's WPA2-PSK and WPA3-SAE networks through NetworkManager on every band the card and regulatory domain allow, and a 1 GiB fetch from the rig host at 2 m runs at 50% or better of Fedora's throughput on the same machine
- [ ] after each of 100 s2idle cycles on the reference laptop, Wi-Fi reassociates and reaches the rig host within 5 s of resume
- [ ] the Phase 32 idle-power line still holds with Wi-Fi associated and power save on
- [ ] the rig's BLE injector pairs as a keyboard and a mouse with LE Secure Connections on each reference machine, drives the terminal and the pointer, and reconnects after a reboot without pairing again
- [ ] the rig's A2DP sink receives the 1 kHz tone as SBC from the reference laptop for 1 hour with no gap over 20 ms, appears and vanishes as a PipeWire device, and its play and pause commands reach the player through AVRCP

§31.7:
- [ ] the HID injector's BLE keyboard and mouse mode

§35.2:
- [ ] captures from the rig's access points, taken in monitor mode on Fedora, replayed as host tests

§35.5:
- [ ] host tests that replay btsnoop captures of real pairings through the kernel's HCI and SMP code

§35.6, Stretch:
- [ ] Wi-Fi 7 and multi-link operation on an Intel BE200
- [ ] access point mode on the AX210, to share the laptop's connection

Phase 36 exit gate, before the tag line:
- [ ] a WebRTC call in the browser between the reference laptop and the desktop, or a vibeOS guest on the rig host until the desktop exists, over the rig's Wi-Fi keeps camera, microphone, and audio output live for 10 minutes with under 1% frame loss in the browser's own statistics

§37.1:
- [ ] the scripted day roams between the rig's access points

### Audio, camera, and removable-media peripherals

**Buy.** A USB audio interface and a TRRS loopback cable for the rig, a USB Audio Class 2 headset, two
USB UVC cameras, a CI-controlled USB switch, and FAT32 and exFAT USB sticks.

**Cost.** About $400, and about $30 for the sticks (2026 estimates). Needs the reference laptop, and the
display peripherals' capture device for its HDMI and DP audio line.

**Unlocks.** Real jacks, HDMI and DP audio, UAC2 headsets, and UVC webcams, recorded and checked by the
rig; removable media through a real USB switch. The desktop and aarch64 desktop goals repeat the headset
and camera lines on their machines.

**Lines.** Phase 34 exit gate, before the tag line:
- [ ] on the reference laptop the tone plays through the headphone jack and through HDMI or DP audio, is recorded on the rig's audio interface and capture device, and passes the host check
- [ ] on the reference laptop, with the rig's cable in the headphone jack, the jack control reads plugged and PipeWire routes playback to the jack rather than the speakers; a USB headset plugged in through the rig's USB switch takes the playing streams within 500 ms, and unplugging it moves them back
- [ ] a USB Audio Class 2 headset plays and records at 48 kHz on the reference laptop
- [ ] round-trip latency, the headphone output looped to the combo jack's microphone input, under 20 ms at PipeWire's default quantum on the reference laptop
- [ ] 8 hours of playback during a parallel kernel build on the reference laptop, with no xrun in PipeWire's counters
- [ ] a USB UVC camera plugged into the reference laptop streams 1080p at 30 frames per second for 10 minutes with under 1% of frames dropped, from the V4L2 sequence numbers; `v4l2-compliance` fails nothing on it that it passes on Fedora on the same machine

§34.2:
- [ ] HDMI and DP audio through the HDMI codec, with the ELD from the **Display links** subsection

§34.3:
- [ ] USB Audio Class 2: clock sources, alternate settings, and feedback endpoints for asynchronous devices

§34.7, Stretch:
- [ ] multichannel HDMI audio, and compressed passthrough to a receiver

Phase 36 exit gate, before the tag line:
- [ ] the file manager mounts a FAT32 and an exFAT USB stick inserted through the rig's USB switch, copies 1 GiB to vibefs and back with matching checksums, and ejects the stick safely

§37.1, once the display peripherals goal is met:
- [ ] the scripted day's docking and undocking also move its USB devices through the rig's USB switch

### Desktop

**Buy.** A mini PC with the laptop's Intel Core Ultra generation, two DisplayPort outputs, an Ethernet
NIC §20.6 drives, and an AX210 from the wireless rig. For the rig: its HID injector, a USB 3 debug cable
if it has no UART, and a switched outlet. Nothing, if the x86_64 test PC qualifies.

**Cost.** About $800, and about $80 for its rig parts (2026 estimates). Needs the display peripherals for
its monitors, the wireless rig for its AX210 and Phase 35 line, and the audio, camera, and
removable-media goal for its Phase 34 line.

**Unlocks.** Two monitors on the same native drivers, a wired network, and a machine with no battery in
the daily-driver tier, against Fedora on the same machine.

**Lines.** Elsewhere in this file:
- the Era VII **Reference machines** paragraph: the desktop proves two monitors on the laptop's drivers, a wired network, and no battery, from Phase 32 on

§31.7:
- [ ] the desktop in the rig: switched power and serial or xHCI debug capture, a HID injector, and the pinned Fedora Workstation image; the x86_64 test PC, if it is the desktop, keeps its existing power control and serial

Phase 32 exit gate, before the tag line:
- [ ] every monitor on the desktop runs at its native mode through the native driver, the display peripherals' hotplug line passes on each of its outputs, and IGT's KMS list passes on it, failing none that passes on Fedora on the same machine

Phase 33 exit gate, before the tag line:
- [ ] the dEQP and IGT `xe` lines pass on the desktop, against Fedora on the same machine

Phase 34 exit gate, before the tag line:
- [ ] the UAC2 headset and USB UVC camera lines pass on the desktop

Phase 35 exit gate, before the tag line:
- [ ] the desktop passes the reference laptop's Wi-Fi join and throughput line through its AX210

Phase 36 exit gate, before the tag line:
- [ ] the display-manager, browser web-platform-tests, Speedometer 3, and video lines pass on the desktop, against Fedora on the same machine

Phase 37 exit gate, before the tag line:
- [ ] the installer, 24-hour workload, 30-night nightly, boot and build time, and unattended-update lines pass on the desktop, and the desktop joins the daily-driver tier

### aarch64 desktop

**Buy.** Nothing beyond the aarch64 server, its HID injector (in the reference laptop's rig parts), the
display peripherals' capture device, and the wireless and audio goals' AX210 on a PCIe adapter, USB
headset, and USB camera.

**Cost.** None of its own.

**Unlocks.** The Era VII stack on aarch64 hardware used as a desktop: a native driver for its display
controller, input, audio, camera, and Wi-Fi, rendering with `llvmpipe`.

**Lines.** Elsewhere in this file:
- the Era VII **Reference machines** paragraph: the aarch64 server, with an AX210 on a PCIe adapter, a USB camera, and a USB headset, is the aarch64 desktop, rendering with `llvmpipe`

Phase 31 exit gate, before the tag line:
- [ ] the rig's HID injector, as a USB precision touchpad, drives tap, two-finger scroll, pinch, and two-finger right click through libinput on the aarch64 server; its idle USB devices autosuspend and PCIe devices reach D3 wherever Fedora reaches them on it

Phase 32, a new subsection before the Stretch, **aarch64 display**:
- [ ] the aarch64 server's display controller as a native driver on §32.1: the ASPEED BMC's on an Ampere machine, with Linux's `ast` driver as the reference, or the HVS and HDMI on a Raspberry Pi 5

Phase 32 exit gate, before the tag line:
- [ ] the aarch64 server's monitor at the best mode its controller offers, with the reference-image comparison through the rig's capture device

Phase 33 exit gate, before the tag line:
- [ ] `llvmpipe` from Alpine renders `kmscube` on the aarch64 server's native display

Phase 34 exit gate, before the tag line:
- [ ] the UAC2 headset plays and records at 48 kHz, and a USB UVC camera streams 1080p30, on the aarch64 server

Phase 35 exit gate, before the tag line:
- [ ] the aarch64 server passes the reference laptop's Wi-Fi join and throughput line through an AX210 on a PCIe adapter

Phase 36 exit gate, before the tag line:
- [ ] the §36.2 desktop starts from its display manager on the aarch64 server

Phase 37 exit gate, before the tag line:
- [ ] the §37.1 workload on the aarch64 server as a desktop, without its suspend and dock steps, since no line suspends it and it has no USB-C display output

### Apple Silicon Macs

**Buy.** An M1 MacBook Air (2020) and an M1 Mac mini (2020), used; the parts for a USB-C debug cable for
each; for the rig, a HID injector each, the Air's lid magnet and power-button actuator, and switched
outlets for both. Later Macs, a second stage, about $700 to $1,500 each used (2026 estimate).

**Cost.** About $1,100 (2026 estimate). Needs the reference laptop, the display peripherals, the wireless
rig, and the audio, camera, and removable-media goal, whose lines and rig equipment its gate reuses.

**Unlocks.** An aarch64 laptop on bare metal, native hardware of the kind the dev host is: Apple's DCP
display, AGX GPU, SPI keyboard and trackpad, SMC, and BCM4378 Wi-Fi and Bluetooth. The Mac mini in
hardware CI, loading each kernel over USB through m1n1's proxy, driven by a self-hosted runner on the rig
host (see **Self-hosted runners**). The Era VII laptop lines rerun on Apple hardware against Fedora Asahi
Remix.

**Lines.** The Apple Silicon phase, restored as it stands in `docs/ROADMAP.md` at commit `ab67d87`: its
Goal, Unlocks, Architectures, Exit gate, and its ten subsections (boot and installation; cores,
interrupts, time; coprocessors and power domains; DMA, storage, buses; laptop platform; display; GPU;
audio; wireless; and the stretch of later Macs and Apple engines), without its Budget paragraph. It is
listed in Era VII after Phase 37 and numbered after the last phase then in force, so no tagged phase is
renumbered; its tag, its section numbers, and its references to them follow the new number. In its gate,
"the Mac mini runs the §20.8 nightly job" becomes "the Mac mini runs a nightly hardware job under a
self-hosted runner on the rig host", and the Phase 31 to 37 lines it names are the reference laptop
goal's, the display peripherals' (the hotplug and lock-frame lines), the wireless rig's (the Wi-Fi and
Bluetooth lines and the call), the audio, camera, and removable-media goal's (the headphone-jack and
latency lines and the call's USB camera), and Phase 36's own browser, accessibility, and input-method
lines. Its gate, with that edit and its own subsections named rather than numbered:
- [ ] both Macs boot vibeOS from internal NVMe, installed beside macOS in its own APFS container, and reach the login prompt on the built-in display (MacBook Air) or HDMI (Mac mini) and on the debug UART
- [ ] macOS still boots on both after the install, and the uninstaller returns the disk to its prior partition layout
- [ ] root on the internal NVMe of both Macs, and the Phase 7 concurrent read-write test passes on each; the §8.5 crash-consistency test passes on the Mac mini, with the rig's outlet cutting power at randomized points during the write workload
- [ ] every DMA-capable device sits behind its DART, and a DMA outside a mapping is stopped and reported as §18.1 reports it
- [ ] all eight cores online, with the performance and efficiency clusters named in `cpus`; `poweroff` and `reboot` work on both
- [ ] the Mac mini's Ethernet, and a USB Ethernet adapter (§20.6) on the MacBook Air, each carry a 1 GiB fetch from the rig host with no corruption
- [ ] the Mac mini runs a nightly hardware job under a self-hosted runner on the rig host: the rig host loads each built kernel over USB through m1n1's proxy, captures the UART, and power-cycles the machine
- [ ] on the MacBook Air, the Phase 31 lines for s2idle cycles, suspended drain, lid, power button, AC, and input pass, measured against Fedora Asahi Remix on the same machine
- [ ] the Phase 32 lines for native resolution, backlight, flips, and resume pass through DCP on the MacBook Air's panel, and the hotplug line on the Mac mini's HDMI
- [ ] Mesa's `asahi` and `honeykrisp` drivers from a pinned Mesa release run unmodified on both Macs, and the Phase 33 dEQP, `glmark2`, and hang lines pass against Fedora Asahi Remix on the same machine
- [ ] the Phase 34 speaker, headphone-jack, microphone, and latency lines pass on the MacBook Air, and its log shows the speaker amplifiers never ran without its audio subsection's speaker protection
- [ ] the Phase 35 Wi-Fi and Bluetooth hardware lines pass on both Macs over the BCM4378
- [ ] the Phase 36 session, lock, browser, accessibility, and input-method lines pass on the MacBook Air, with a USB camera for the call, and the lock line's first frame taken from its display subsection's readback of the first surface after resume instead of the rig's capture
- [ ] the Phase 37 workload, s2idle, and battery-life lines pass on the MacBook Air against Fedora Asahi Remix, the workload without its external-display steps, and it joins the daily-driver tier

Elsewhere in this file:
- How to read this: "Forty phases" becomes "Forty-one phases"
- the arc: a row for the phase in Era VII, "An Apple Silicon laptop"; the closing paragraph's "none of 26 to 29 or 31 to 37" and the Era VIII preamble's "31 to 37" gain its number
- the Era VII preamble: the phase needs 37, because its gate reruns this era's laptop lines on Apple hardware, while its bring-up (its boot, core, coprocessor, and DMA subsections) needs only 31 and §18.1; lines that need an aarch64 laptop are gated there, on the MacBook Air
- the Era VII **Reference machines** paragraph: the MacBook Air proves the aarch64 laptop Asahi Linux documents most completely, and the Mac mini the same SoC with HDMI, Ethernet, and m1n1's USB proxy for hardware CI
- Beyond, the second stage: **Later Apple Silicon** (after the Apple Silicon phase): Apple Silicon machines past the M1 MacBook Air and Mac mini, such as the M1 Pro, Max, and Ultra and the M2 and later generations, as Asahi Linux documents them

### AMD graphics, Thunderbolt docks, and a discrete GPU

**Buy.** An AMD laptop with RDNA3 graphics, a Thunderbolt or USB4 dock, and a discrete GPU with VRAM.

**Cost.** About $1,300 to $1,800: about $800 to $1,200 for the laptop, $200 to $300 for the dock, and
$250 to $350 for an RDNA3 Radeon card (2026 estimates).

**Unlocks.** A second display engine and render driver family, USB4 tunnels through native hotplug, and
VRAM management.

**Lines.** §32.5, Stretch:
- [ ] AMD's DCN display engine on an AMD laptop: PSP and SMU bring-up for display clocks, atomfirmware tables, and the DMCUB firmware, from AMD's published register headers
- [ ] Thunderbolt and USB4 docks: the USB4 connection manager, PCIe tunnels through §20.9's native hotplug, and their DMA confined by §18.1

§33.4, Stretch:
- [ ] the `amdgpu` uapi that `radeonsi` and `radv` use, on an AMD laptop's RDNA3 graphics: GFX and compute rings, SDMA, the interrupt ring, GPU virtual memory, SMU power management, and VCN video
- [ ] a discrete GPU with VRAM: a manager that evicts to system memory, and resizable BAR where the platform allows it

## Other architectures and silicon

### Raspberry Pi 5

**Buy.** A Raspberry Pi 5 with a power supply and an SD card, and an SD mux for switched boots. Worth
buying only once a maintained edk2 port or U-Boot's EFI layer boots Limine on the board in hand; the
original edk2 port was archived in February 2025.

**Cost.** About $100 to $150 (2026 estimate). Needs the x86_64 test PC's rig host for its nightly boots.

**Unlocks.** A board's own device tree and firmware instead of Linux's trees in host tests: BCM2712's
non-ECAM PCIe, the RP1 southbridge, its Cadence GEM MAC, SD root, and USB inside RP1. A self-hosted
runner (see **Self-hosted runners**).

**Lines.** §20.7:
- [ ] a Raspberry Pi 5 whose EFI firmware boots Limine: the BCM2712 PCIe root complex (not ECAM, so §11.5's generic path does not reach it), the RP1 southbridge behind it, and RP1's Cadence GEM MAC, carrying the Phase 20 gate's 1 GiB fetch
- [ ] SD root on the board, USB over RP1's xHCI (shared with §20.3), and serial over the board's UART
- [ ] the device tree the board's firmware passes, compared with the Linux tree §20.7's host tests parse, each difference handled or listed in `docs/HARDWARE.md`

§20.8:
- [ ] the board netboots or boots from a switched SD mux nightly, driven by the rig host's self-hosted runner under the **Self-hosted runners** rules

### riscv64 board

**Buy.** A riscv64 board whose UEFI firmware (edk2 or U-Boot's EFI layer) boots Limine, and a USB serial
adapter.

**Cost.** About $100 to $300 (2026 estimate).

**Unlocks.** The riscv64 port on real hardware, which exposes what QEMU's `virt` machine forgives.

**Lines.** Beyond:
- **riscv64 on a real board** (after §11.8, 20, and the riscv64 entry): the riscv64 port boots on the board with root on its own storage, and a disk test there, a 1 GiB fetch over its NIC with no corruption, and a USB keyboard read through its xHCI all pass.

### MTE-capable aarch64 machine

**Buy.** A UEFI-booting aarch64 machine whose CPU implements MTE. The Altra-class server does not: its
Neoverse N1 cores lack it. The AmpereOne-class server's cores implement it, so buying that server meets
this goal. Confirm that the firmware enables MTE before buying.

**Cost.** About $200 to $500 for a board such as Radxa's Orion O6 (CIX P1), whose edk2 firmware can turn
MTE on (2026 estimate); nothing of its own once the AmpereOne-class server goal is met.

**Unlocks.** §18.4's tag-based KASAN checked by real tag hardware, with asynchronous tag checking on in
release images.

**Lines.** Beyond:
- **Memory tagging on silicon** (after 18 and 20): §18.4's MTE build booted bare metal on an MTE-capable aarch64 machine, with asynchronous tag checking on in release images.

### AmpereOne-class aarch64 server

**Buy.** An AmpereOne-class server, whose cores have FEAT_NV2 and MTE. Bought first, it replaces the
Altra-class server and covers all of that goal's lines. It also meets the MTE-capable aarch64 machine
goal, by the same edit, if that goal is still open.

**Cost.** About $10,000 to $25,000; the CPU alone lists near $5,000 (2026 estimates).

**Unlocks.** FEAT_NV2 on silicon, so Phase 21's aarch64 nesting runs a guest hypervisor on real cores
rather than only under TCG with `virtualization=on` and in the dev host's HVF record. MTE on silicon, as
the MTE-capable aarch64 machine goal describes.

**Lines.** §20.8:
- [ ] the AmpereOne-class server's nightly record reports FEAT_NV2, which Phase 21's nesting lines read beside the hosted x86_64 runners' vendor records

§21.3:
- [ ] aarch64 nesting at hardware speed: on the AmpereOne-class server booted bare metal at EL2, vibeOS runs a vibeOS guest at virtual EL2 through FEAT_NV2, and that guest boots a third vibeOS to `shell ready`

Phase 21 exit gate, before the tag line:
- [ ] on the AmpereOne-class server, the same three-level boot also runs with Linux KVM as the host, booted with `kvm-arm.mode=nested` (Linux 6.16 or later), and both boot times are recorded in the §21.3 comparison

### Morello board

**Buy.** An Arm Morello board. Not sold retail: boards went out through Arm's Morello research program.

**Cost.** No list price (2026), since boards went out through Arm's research program; availability and
price to confirm.

**Unlocks.** Capability hardware under the CHERI port, in place of the CHERI QEMU.

**Lines.** Beyond:
- **CHERI on silicon** (after 11, 18, and the CHERI capabilities entry): the CHERI port booted on an Arm Morello board, with every kernel and user pointer a bounded capability.

### CXL hardware

**Buy.** A CXL-capable server platform and a CXL Type 3 memory expander.

**Cost.** Several thousand dollars (2026 estimate; confirm before buying).

**Unlocks.** Tiering measured at real CXL latency instead of under QEMU's emulation.

**Lines.** Beyond:
- **CXL memory tiering on hardware** (after 27 and the CXL memory tiering entry): the tiering policy on a CXL Type 3 memory expander in a CXL-capable server, with promotion and demotion measured against Linux's on the same machine.

## More desktop hardware

Each adds one Beyond entry. The peripherals plug into a reference machine or a test machine; the
laptops are machines of their own.

### USB fingerprint reader

**Buy.** A USB fingerprint sensor that libfprint supports. **Cost.** About $30 to $60 (2026 estimate).
**Unlocks.** Fingerprint login and `sudo`.

**Lines.** Beyond:
- **Fingerprint readers** (after 36): `libfprint` sensors through `fprintd` on a reference machine, used for the session's login and for `sudo`.

### Network printer and scanner

**Buy.** An IPP Everywhere and eSCL multifunction printer. **Cost.** About $150 to $250 (2026 estimate).
**Unlocks.** Scanning, and printing to a physical device instead of `ippeveprinter`.

**Lines.** Beyond:
- **Printing and scanning on devices** (after 36 and the printing entry): IPP Everywhere and eSCL through CUPS and `sane-airscan` against a physical multifunction printer, which covers most network printers and scanners sold in the last decade.

### IPU6 laptop

**Buy.** A recent Intel laptop whose camera sits behind IPU6; nothing if the reference laptop's does.
**Cost.** About $1,000 (2026 estimate). **Unlocks.** The built-in cameras of most recent Intel laptops.

**Lines.** Beyond:
- **MIPI cameras** (after 34 and the media-controller cameras entry): Intel's IPU6 and later, through which most recent Intel laptops route their cameras, with `libcamera` and its software ISP as the userspace.

### Convertible laptop

**Buy.** A convertible that Linux supports, with an I2C-HID touchscreen, a stylus digitizer, and a sensor
hub. **Cost.** About $1,000 (2026 estimate). **Unlocks.** Touch and pen input on real I2C-HID hardware, with
rotation from the sensor hub.

**Lines.** Beyond:
- **Touchscreens and pens** (after 31): I2C-HID touchscreens and stylus digitizers with pressure and tilt, and rotation from the sensor hub, on a convertible added to the reference machines.

### NVIDIA GPU

**Buy.** A used Turing-or-later NVIDIA card, for a reference machine or test machine with a free x16
slot. **Cost.** About $200 to $400 (2026 estimate). **Unlocks.** NVIDIA GPUs through upstream Mesa's NVK.

**Lines.** Beyond:
- **NVIDIA GPUs** (after 33): Turing and later through the GSP firmware, as Linux's `nova` driver does, with the render uapi Mesa's NVK runs on unmodified.

### Hybrid-graphics laptop

**Buy.** A laptop with integrated and discrete GPUs. **Cost.** About $1,500 (2026 estimate). **Unlocks.**
Render offload to a discrete GPU, powered off when idle.

**Lines.** Beyond:
- **Hybrid graphics** (after 33): a laptop that renders on a discrete GPU and scans out on the integrated one, with the discrete GPU powered off when idle.

### Snapdragon X laptop

**Buy.** A machine in the ThinkPad T14s Gen 6 class. **Cost.** About $1,200 to $1,500 (2026 estimate).
**Unlocks.** A second aarch64 laptop family.

**Lines.** Beyond:
- **Snapdragon X laptops** (after 37): a second aarch64 laptop family, such as the ThinkPad T14s Gen 6: device tree, Adreno through Mesa's `freedreno`, `ath12k` Wi-Fi, and Qualcomm's remote processors.

## Paid services

### Model API credits

**Open.** A model API key issued by the maintainer's account for one project on the provider's side,
with pay-as-you-go billing under a spend cap, kept as the only secret of a `model-api` GitHub
environment that admits deployments from `main` alone and that only the replay workflow names, so a
workflow a branch adds cannot read it. The agent on vibeOS gets a separate key, which never enters a CI
guest.

**Cost.** The provider's list price per token (2026) for roughly the tokens each replayed phase took to
build, held under a monthly spend limit the maintainer sets.

**Unlocks.** Gate replay on a schedule for each new model without the maintainer's own sessions, and an
agent on a vibeOS machine with a key of its own.

**Lines.**
- Beyond, **Gate replay**: its last sentence becomes "It is rerun by a scheduled workflow on `main` on each new model release, with the API key of the `model-api` environment."
- Beyond, **Agents on vibeOS**: its last sentence becomes "It uses an API key of its own rather than the maintainer's agent account."

### Khronos conformance submission

**Open.** Khronos Adopter status for Vulkan, OpenGL, and OpenGL ES.

**Cost.** $120,000 for Vulkan, $60,000 for OpenGL 3.2 to 4.6, and $30,000 for OpenGL ES 1.1 to 3.2 for
non-members, $210,000 in all, from Khronos's adopters page (2026), which prices each API on its own and
lists no open-source waiver.

**Unlocks.** An official conformance claim for the render stack. The free Phase 33 lines already run the
same tests without the claim.

**Lines.** §33.3:
- [ ] a Khronos conformance submission for vibeOS with `lavapipe` (Vulkan) and `llvmpipe` (OpenGL ES and OpenGL), run with VK-GL-CTS in its official configuration, since `deqp-runner` results are not a conformance result

---

*Living document. Phases get reordered, split, and abandoned as the experiment finds out what is
actually hard. When that happens, edit this file rather than adding a note explaining why it is wrong.*
