//! `avocado vm` subcommands.
//!
//! Thin clap/dispatch layer over [`crate::utils::vm::lifecycle`]. The actual
//! work lives in the utils submodules; these modules just present the user-
//! facing flags and pretty-print results.

pub mod logs;
pub mod rebuild;
pub mod shell;
pub mod start;
pub mod status;
pub mod stop;
pub mod update;
