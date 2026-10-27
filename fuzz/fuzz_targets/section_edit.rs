#![no_main]

use libfuzzer_sys::fuzz_target;

// The first three bytes pick the edit and its arguments; the rest is the
// document, then an optional NUL and the inserted body.
fuzz_target!(|data: &[u8]| {
    let Some((controls, rest)) = data.split_first_chunk::<3>() else {
        return;
    };
    let Ok(input) = std::str::from_utf8(rest) else {
        return;
    };
    hallouminate_fuzz::check_section_edit(*controls, input);
});
