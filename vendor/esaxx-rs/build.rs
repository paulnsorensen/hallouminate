// Vendored from esaxx-rs 0.1.10 with one change vs upstream: `.static_crt(true)`
// -> `.static_crt(false)` in both arms, so the C++ object links against the
// DYNAMIC MSVC CRT (/MD). Upstream's /MT objects fail to link against ort's
// /MD onnxruntime prebuilt on x86_64-pc-windows-msvc (LNK2038 -> LNK1319).
// On non-Windows targets `static_crt` is a no-op, so this is byte-equivalent
// to upstream everywhere except windows-msvc. See #48 / Narsil/esaxx-rs PR #19.

#[cfg(feature = "cpp")]
#[cfg(not(target_os = "macos"))]
fn main() {
    cc::Build::new()
        .cpp(true)
        .flag("-std=c++11")
        .static_crt(false)
        .file("src/esaxx.cpp")
        .include("src")
        .compile("esaxx");
}

#[cfg(feature = "cpp")]
#[cfg(target_os = "macos")]
fn main() {
    cc::Build::new()
        .cpp(true)
        .flag("-std=c++11")
        .flag("-stdlib=libc++")
        .static_crt(false)
        .file("src/esaxx.cpp")
        .include("src")
        .compile("esaxx");
}

#[cfg(not(feature = "cpp"))]
fn main() {}
