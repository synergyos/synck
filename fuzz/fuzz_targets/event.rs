#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| synck_fuzz::event(data));
