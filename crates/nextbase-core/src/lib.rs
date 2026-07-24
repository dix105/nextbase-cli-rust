//! Shared core for the Nextbase CLI tools.
//!
//! Everything here reads and writes the *existing* `~/.wisper-cli/` files so the
//! Rust build and the TypeScript build can run side by side during the port. Do
//! not change the on-disk formats without a migration.

pub mod config;
pub mod log;
pub mod paths;
pub mod shortcut;
pub mod storage;
pub mod verify;
