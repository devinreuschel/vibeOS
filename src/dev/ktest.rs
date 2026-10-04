//! In-guest tests for dev (kernel_tests only). Rows: [`TESTS`].

use crate::ktest::{Test, test};

#[cfg(target_arch = "x86_64")]
mod claims;
mod dma;
mod hooks;
#[cfg(target_arch = "aarch64")]
mod mmio;
mod pci;
mod rng;

#[cfg(target_arch = "x86_64")]
pub(crate) use claims::*;
pub(crate) use dma::*;
pub(crate) use hooks::*;
#[cfg(target_arch = "aarch64")]
pub(crate) use mmio::*;
pub(crate) use pci::*;
pub(crate) use rng::*;

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
#[cfg(target_arch = "x86_64")]
pub(crate) const TESTS: &[Test] = &[
    test("pci_qemu_set", test_pci_qemu_set),
    test("pci_scan_bsp_only", test_pci_scan_bsp_only),
    test("pci_bar_map", test_pci_bar_map),
    test("pci_cfg_rw", test_pci_cfg_rw),
    test("pci_claim_exclusive", test_pci_claim_exclusive).once(),
    test("pci_bind_order", test_pci_bind_order).once(),
    test("dma_alloc", test_dma_alloc),
    test("dma_edu", test_dma_edu),
    test("virtio_bind", test_virtio_bind),
    test("virtio_vq", test_virtio_vq),
    test("dev_random_source", test_dev_random_source),
    test("dev_random_eagain", test_dev_random_eagain),
    test("rng_pool_no_dup", rng_pool_no_dup),
    test(
        "rng_refill_after_empty_completion",
        rng_refill_after_empty_completion,
    ),
    test("dev_probe_alloc_fail", test_dev_probe_alloc_fail),
    test("rng_second_probe_refused", rng_second_probe_refused),
    test("dev_bar_claims", test_dev_bar_claims).once(),
    test("map_mmio_refuses_ram", test_map_mmio_refuses_ram),
];

#[cfg(target_arch = "aarch64")]
pub(crate) const TESTS: &[Test] = &[
    test("dma_alloc", test_dma_alloc),
    test("dma_edu", test_dma_edu),
    test("virtio_bind", test_virtio_bind),
    test("virtio_vq", test_virtio_vq),
    test("block_vblk_mmio_smp", test_block_vblk_mmio_smp),
    test("dev_random_source", test_dev_random_source),
];
