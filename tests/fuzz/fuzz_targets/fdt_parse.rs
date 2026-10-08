#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| vibeos_fuzz::fdt_parse(data));
