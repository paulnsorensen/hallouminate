# Vendored `esaxx-rs` (patched for windows-msvc)

This is a verbatim copy of [`esaxx-rs` 0.1.10](https://github.com/Narsil/esaxx-rs)
(Apache-2.0) with a **single** change in `build.rs`:

```diff
-        .static_crt(true)
+        .static_crt(false)
```

## Why

Upstream hardcodes the **static** MSVC C runtime (`/MT`) for its C++ object.
`ort`'s `download-binaries` ships a prebuilt onnxruntime built against the
**dynamic** CRT (`/MD`). The MSVC linker refuses to mix the two:

```
error LNK2038: mismatch detected for 'RuntimeLibrary':
  value 'MT_StaticRelease' doesn't match value 'MD_DynamicRelease'
  in libort_sys-*.rlib(onnxruntime_c_api.obj)
fatal error LNK1319: 1 mismatches detected
```

No `RUSTFLAGS`, `CFLAGS`, or `.cargo/config.toml` setting can override the
explicit `.static_crt(true)` call in upstream's `build.rs`, so the crate has to
be patched. Switching it to `/MD` matches the onnxruntime prebuilt and links.
`static_crt` is a no-op off windows-msvc, so unix builds are unaffected.

## How it's wired

`[patch.crates-io]` in the repository root `Cargo.toml` redirects the
transitive `fastembed`/`text-splitter` -> `tokenizers` -> `esaxx-rs` dependency
at this path.

## Upstream

Tracked by [Narsil/esaxx-rs#19](https://github.com/Narsil/esaxx-rs/pull/19)
("Use dynamic C runtime (MD) for Windows MSVC"). Drop this vendor and the
`[patch.crates-io]` entry once a fixed `esaxx-rs` is published to crates.io.

Only the library is vendored; upstream's example, benchmark, and 392 KB bench
corpus are omitted as unused.
