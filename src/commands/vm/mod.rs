//! `avocado vm` subcommands.
//!
//! Thin clap/dispatch layer over [`crate::utils::vm::lifecycle`]. The actual
//! work lives in the utils submodules; these modules just present the user-
//! facing flags and pretty-print results.

pub mod config;
pub mod connect;
pub mod logs;
pub mod rebuild;
pub mod reset;
pub mod rubicon;
#[cfg(target_os = "linux")]
pub mod service;
pub mod shell;
pub mod start;
pub mod status;
pub mod stop;
#[cfg(unix)]
pub mod supervise;
pub mod update;
