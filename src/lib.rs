mod autostart;
mod capture;
pub mod capture_test;
pub mod client;
pub mod config;
mod connect;
mod crypto;
mod dns;
mod emulation;
pub mod emulation_test;
// Global hotkeys read Linux evdev devices directly. The evdev module is
// Linux-only; `hotkey_task` is compiled everywhere and falls back to a no-op
// listener elsewhere, so the service can reference it without `#[cfg]`.
#[cfg(all(unix, not(target_os = "macos")))]
mod hotkey;
mod hotkey_task;
mod listen;
mod remap;
pub mod service;
