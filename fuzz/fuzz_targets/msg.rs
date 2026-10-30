#![no_main]

// `msg_parser` panics on hostile files and the importer contains that. libFuzzer's
// hook would abort first, so only a panic that escapes the importer counts here.
libfuzzer_sys::fuzz_target!(
    init: std::panic::set_hook(Box::new(|_| {})),
    |data: &[u8]| synck_fuzz::msg(data)
);
