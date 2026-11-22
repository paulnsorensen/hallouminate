//! Marks a crates.io build as a release build.
//!
//! `cargo package` writes `.cargo_vcs_info.json` into every packaged crate,
//! so its presence means that this source came from a published package, not
//! from a git checkout. An explicit `HALLOUMINATE_RELEASE_CHANNEL` wins.

use std::path::PathBuf;

const MARKER: &str = "HALLOUMINATE_RELEASE_CHANNEL";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={MARKER}");
    if std::env::var_os(MARKER).is_some_and(|value| !value.is_empty()) {
        return;
    }
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    if PathBuf::from(manifest_dir)
        .join(".cargo_vcs_info.json")
        .is_file()
    {
        println!("cargo:rustc-env={MARKER}=crates-io");
    }
}
