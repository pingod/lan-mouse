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
// The hotkey matcher (key sampling, chord state, tap synthesis) is only needed
// where a listener feeds it: evdev on Linux, the Quartz event tap on macOS.
// `hotkey_task` itself is compiled everywhere so the service can reference its
// field without `#[cfg]`; on other platforms it is a no-op listener.
#[cfg(any(unix, windows))]
mod hotkey;
mod hotkey_task;
mod listen;
pub mod logging;
mod remap;
pub mod service;
pub mod sysinfo;
