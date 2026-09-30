#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| vibeos_fuzz::pci_enumerate(data));
