#![no_main]

use libfuzzer_sys::fuzz_target;

// The first byte picks the file type and the chunk budget; the rest of the
// input is the file.
fuzz_target!(|data: &[u8]| {
    hallouminate_fuzz::check_format_handlers(data);
});
