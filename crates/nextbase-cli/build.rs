//! Link the binaries against the OS Swift runtime path on macOS.
//!
//! ScreenCaptureKit capture (see `nextbase-core/src/capture/system/macos.rs`) pulls in
//! Swift runtime libraries such as `libswift_Concurrency.dylib`, which live in
//! `/usr/lib/swift`. Without an rpath pointing there the binary links fine and then
//! dies at startup with "Library not loaded … no LC_RPATH's found" — on the user's
//! machine, not ours.
//!
//! The *search* path for the Swift compatibility archives is emitted by
//! `nextbase-core/build.rs`, because `rustc-link-search` propagates to dependents
//! while `rustc-link-arg-bins` does not.
//!
//! This lives here rather than in `.cargo/config.toml` so a build with `RUSTFLAGS` set
//! cannot silently drop it.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/usr/lib/swift");
    }
}
