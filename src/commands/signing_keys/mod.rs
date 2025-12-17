//! Signing keys management commands.
//!
//! Provides commands for creating, listing, and removing signing keys
//! stored in the global avocado configuration.

mod create;
mod list;
mod remove;

// These exports are used by the binary target (main.rs) but not the library target,
// which causes clippy warnings in the lib build. We allow unused_imports here.
#[allow(unused_imports)]
pub use create::SigningKeysCreateCommand;
#[allow(unused_imports)]
pub use list::SigningKeysListCommand;
#[allow(unused_imports)]
pub use remove::SigningKeysRemoveCommand;
