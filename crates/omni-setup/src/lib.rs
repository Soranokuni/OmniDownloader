//! `OmniIngest-Setup.exe` (P13.1b): one executable that carries the release
//! folder as an appended, hash-checked zip, elevates itself, and installs,
//! upgrades, changes the service account of, or removes OmniIngest through
//! the installer in `omni_cli::install`. Its pages (P13.1c) are the flow in
//! [`flow`] over the [`ui::Ui`] and [`backend::Backend`] traits; the Win32
//! dialogs themselves live in the binary (`main.rs`), never in this library.

pub mod args;
pub mod backend;
pub mod elevate;
pub mod exit;
pub mod flow;
pub mod log;
pub mod payload;
pub mod real;
pub mod registry;
pub mod removal;
pub mod run;
pub mod texts;
pub mod ui;
pub mod workdir;
