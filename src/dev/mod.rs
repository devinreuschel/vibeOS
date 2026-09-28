//! Devices: the kernel half of subsystem `dev` (DESIGN §1.3).

pub(crate) mod dev_init;
pub(crate) mod dma_init;
pub(crate) mod entropy_init;
pub(crate) mod pci_init;
pub(crate) mod virtio_init;
