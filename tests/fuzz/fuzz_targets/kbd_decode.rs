#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| vibeos_fuzz::kbd_decode(data));
