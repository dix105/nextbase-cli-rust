//! macOS link setup for the Swift code that ScreenCaptureKit capture pulls in.
//!
//! `screencapturekit` compiles a Swift bridge whose objects reference
//! `__swift_FORCE_LOAD_$_swiftCompatibility56` and friends. Those archives live in
//! the developer directory, but the dependency's own build script only looks inside
//! `XcodeDefault.xctoolchain` — which does not exist on a machine with only the
//! Command Line Tools, where they sit at `usr/lib/swift/macosx` instead.
//!
//! The search path is emitted here rather than in `nextbase-cli` because
//! `rustc-link-search` propagates to everything that links this crate — the two
//! binaries *and* every test binary. `rustc-link-arg-bins` would not reach the tests.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // Note: the `/usr/lib/swift` *rpath* is only emitted for the binaries, in
    // `nextbase-cli/build.rs`. `rustc-link-arg-tests` is not a stable build-script
    // instruction, and no test loads the Swift runtime — the ScreenCaptureKit path is
    // exercised through `nbmeet doctor` and a real recording. A test that did would
    // fail loudly at startup with a dyld "no LC_RPATH's found" message.
    match swift_static_libs() {
        Some(directory) => println!("cargo:rustc-link-search=native={}", directory.display()),
        // A full Xcode install lets the dependency find these itself, so this is a
        // warning rather than a failure — but an unexplained undefined-symbol link
        // error is much worse than a note here.
        None => println!(
            "cargo:warning=Could not locate libswiftCompatibility56.a. If linking fails with __swift_FORCE_LOAD_$_swiftCompatibility56, run: xcode-select --install"
        ),
    }
}

/// Where this machine keeps `libswiftCompatibility*.a`.
///
/// Command Line Tools put them under `usr/lib/swift/macosx`; a full Xcode install
/// nests them inside `Toolchains/XcodeDefault.xctoolchain`.
fn swift_static_libs() -> Option<PathBuf> {
    let developer = Command::new("xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()))?;

    [
        developer.join("usr/lib/swift/macosx"),
        developer.join("Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("libswiftCompatibility56.a").exists())
}
