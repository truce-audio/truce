#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| truce_fuzz::ump_decode(data));
