//! Shared core for the Nextbase CLI tools.
//!
//! Everything here reads and writes the *existing* `~/.wisper-cli/` files so the
//! Rust build and the TypeScript build can run side by side during the port. Do
//! not change the on-disk formats without a migration.

pub mod audio;
pub mod autostart;
pub mod config;
pub mod hotkey;
pub mod log;
pub mod media;
pub mod paste;
pub mod paths;
pub mod polish;
pub mod process_state;
pub mod shortcut;
pub mod storage;
pub mod transcribe;
pub mod updater;
pub mod verify;
