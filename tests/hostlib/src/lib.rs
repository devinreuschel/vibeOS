//! Host wrapper. Pulls in the same portable modules as `src/lib.rs`, but
//! compiled against the host toolchain so `cargo test --lib` runs their unit
//! tests without any kernel-target machinery.

// The kernel's own lib.rs also references these modules, so this stays in
// sync automatically.
#[path = "../../../src/acpi.rs"]
pub mod acpi;

#[path = "../../../src/apic.rs"]
pub mod apic;

#[path = "../../../src/fmt_util.rs"]
pub mod fmt_util;

#[path = "../../../src/marker.rs"]
pub mod marker;

#[path = "../../../src/heap.rs"]
pub mod heap;

#[path = "../../../src/kva.rs"]
pub mod kva;

#[path = "../../../src/paging.rs"]
pub mod paging;

#[path = "../../../src/pmm.rs"]
pub mod pmm;

#[path = "../../../src/sched.rs"]
pub mod sched;

#[path = "../../../src/uart.rs"]
pub mod uart;

#[path = "../../../src/vectors.rs"]
pub mod vectors;

#[path = "../../../src/desc.rs"]
pub mod desc;

#[path = "../../../src/pic.rs"]
pub mod pic;

#[path = "../../../src/per_cpu.rs"]
pub mod per_cpu;

#[path = "../../../src/sync.rs"]
pub mod sync;

#[path = "../../../src/thread.rs"]
pub mod thread;

#[path = "../../../src/time.rs"]
pub mod time;

#[path = "../../../src/wait.rs"]
pub mod wait;

#[cfg(test)]
mod smoke {
    use super::marker;

    #[test]
    fn markers_are_lowercase_prefixed() {
        for m in [
            marker::SERIAL_ONLINE,
            marker::LIMINE_OK,
            marker::PMM_PREFIX,
            marker::PAGING_CR3_OK,
            marker::PAGING_MMIO_UC,
            marker::HEAP_OK,
            marker::KVA_READY,
            marker::GDT_OK,
            marker::PIC_REMAPPED,
            marker::IDT_OK,
            marker::PER_CPU_BSP,
            marker::ACPI_XSDT_PREFIX,
            marker::TIME_TSC_PREFIX,
            marker::TIME_LAPIC_PREFIX,
            marker::SCHED_CPU0,
            marker::IRQ_ENABLED,
            marker::BOOT_DONE,
        ] {
            assert!(m.starts_with("vibeOS: "), "marker missing prefix: {m}");
            assert!(!m.ends_with(['.', '!']), "marker has trailing punct: {m}");
        }
        // The PMM line is assembled at runtime: prefix, decimal count,
        // suffix. Confirm the fragments agree with the phase-1 exit-gate
        // string in the roadmap.
        assert_eq!(marker::PMM_PREFIX, "vibeOS: pmm: ");
        assert_eq!(marker::PMM_FREE_SUFFIX, " free 4KiB frames");
        // Paging §1.2 exit marker is a fixed string; must match the
        // harness contract byte-for-byte.
        assert_eq!(marker::PAGING_CR3_OK, "vibeOS: paging: cr3 ok");
        assert_eq!(marker::PAGING_MMIO_UC, "vibeOS: paging: mmio uc");
        assert_eq!(marker::HEAP_OK, "vibeOS: heap ok");
        assert_eq!(marker::KVA_READY, "vibeOS: kva: ready");
        assert_eq!(marker::GDT_OK, "vibeOS: gdt ok");
        assert_eq!(marker::PIC_REMAPPED, "vibeOS: pic: remapped");
        assert_eq!(marker::IDT_OK, "vibeOS: idt ok");
        assert_eq!(marker::PER_CPU_BSP, "vibeOS: per_cpu: bsp ready");
        assert_eq!(marker::ACPI_XSDT_PREFIX, "vibeOS: acpi: xsdt ");
        assert_eq!(marker::ACPI_XSDT_SUFFIX, " tables");
        assert_eq!(marker::TIME_TSC_PREFIX, "vibeOS: time: tsc ");
        assert_eq!(marker::TIME_TSC_SUFFIX, "/ms");
        assert_eq!(marker::TIME_LAPIC_PREFIX, "vibeOS: time: lapic_timer ok (");
        assert_eq!(marker::TIME_LAPIC_SUFFIX, ")");
        assert_eq!(marker::SCHED_CPU0, "vibeOS: sched: cpu0 ready");
        assert_eq!(marker::IRQ_ENABLED, "vibeOS: irq: enabled");
        assert_eq!(marker::BOOT_DONE, "vibeOS: boot: phase1 done");
    }

    #[test]
    fn panic_banner_short_enough_for_uart() {
        // The panic path prints this before allocating anything. Keep it
        // small enough that the DESIGN §9.6 tx-poll cap never bites.
        assert!(marker::PANIC_BANNER.len() < 64);
    }
}
