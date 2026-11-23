#![no_main]

use libfuzzer_sys::fuzz_target;

// The first byte picks a small character budget so splits happen often;
// the rest of the input is the document.
fuzz_target!(|data: &[u8]| {
    let Some((first, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let budget = usize::from(*first % 64) + 1;
    hallouminate_fuzz::check_chunks(text, budget, hallouminate_fuzz::ChunkerKind::Rst);
});
