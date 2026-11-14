mod capture;
pub mod capture_test;
pub mod client;
pub mod config;
mod connect;
mod crypto;
mod dns;
mod emulation;
pub mod emulation_test;
// Global hotkeys read Linux evdev devices directly; other platforms keep
// using the capture-based `[[remap]]` path only.
#[cfg(all(unix, not(target_os = "macos")))]
mod hotkey;
#[cfg(all(unix, not(target_os = "macos")))]
mod hotkey_task;
mod listen;
mod remap;
pub mod service;
