# D3 · Capture boot information once (`BootInfo`)

| | |
|---|---|
| **Area** | 4.3 Data model & state management |
| **Impact / Effort / Phase** | Low / S / I |
| **Depends on** | Q3 (`BootCell`) |
| **Blocks** | A1 (`boot/` directory has content) |
| **Review** | [ARCHITECTURE_REVIEW.md §4.3](../ARCHITECTURE_REVIEW.md#43-data-model--state-management) |
| **Status** | Implemented (this PR). `src/boot.rs` captures Limine once into `BootInfo` (`BootCell`). `pmm_init` / paging / ACPI / FB consume `boot::info()`. |

## Problem

- `docs/ROADMAP.md:43` keeps `- [ ] a BootInfo struct captured once at entry; nothing else reads Limine statics` deferred since Phase 0.
- `src/main.rs::normal_boot_tail` reads `HHDM`, `MEMMAP`, `EXEC_ADDR`, `RSDP` inline; `FRAMEBUFFER` is `pub(crate)` so `src/pmm_init.rs:145` and `src/fb_init.rs:57` read the Limine static later; `memmap_high_water` and `framebuffer_phys_end` live in `main.rs`.

## Recommended fix

One `BootInfo` captured first thing in `normal_boot_tail`, stored write-once, and the only consumer of Limine responses.

## Implementation plan

1. **`src/boot.rs`** (kernel side):
   ```rust
   pub struct FbInfo { pub phys: u64, pub virt: u64, pub width: u32, pub height: u32, pub pitch: u32, pub bpp: u16, pub size: u64 }
   pub struct BootInfo { pub hhdm_offset: u64, pub kernel_phys_base: u64, pub kernel_virt_base: u64,
       pub memmap: &'static [&'static limine::memmap::Entry], pub usable_high_water: u64,
       pub rsdp_phys: u64, pub framebuffers: [Option<FbInfo>; 2] }
   pub fn capture() -> BootInfo   // halts with the existing `halt_with` messages if a response is missing
   pub fn info() -> &'static BootInfo
   ```
   Move the four Limine request statics, `memmap_high_water`, `framebuffer_phys_end`, and the HHDM assertion into `boot.rs`; `main.rs` keeps only `_start`, the base-revision check, and the boot sequence.
2. **Consumers:** `pmm_init::init(boot::info())`, `paging_init::install(...)` takes `kernel_phys_base` and `usable_high_water` from it, `acpi_init::init(info.rsdp_phys)`, `fb_init` uses `info.framebuffers[0]`. Make `FRAMEBUFFER` private.
3. **ROADMAP:** tick §0.3.
4. **Test:** ktest `bootinfo_consistent`: `info().hhdm_offset == paging_init::HHDM_BASE`, `kernel_phys_base` matches `EXEC_ADDR` (test module may read the static), framebuffer size non-zero when Limine provided one.

## Acceptance criteria

- `grep -rn 'FRAMEBUFFER\|HHDM\.\|MEMMAP\.\|RSDP\.\|EXEC_ADDR\.' src --include='*.rs' | grep -v boot.rs` is empty.
- e2e marker order unchanged; `pmm: <n> free 4KiB frames` value unchanged on the same QEMU config.

## Tests

The ktest above; existing e2e.

## Risks and rollback

None material; pure refactor before any allocation happens.
