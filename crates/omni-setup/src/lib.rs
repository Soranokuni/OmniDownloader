//! `OmniIngest-Setup.exe` (P13.1b): one executable that carries the release
//! folder as an appended, hash-checked zip, elevates itself, and installs,
//! upgrades, changes the service account of, or removes OmniIngest through
//! the installer in `omni_cli::install`. `main.rs` only wires the console.

pub mod args;
pub mod elevate;
pub mod exit;
pub mod log;
pub mod payload;
pub mod registry;
pub mod removal;
pub mod run;
pub mod workdir;
