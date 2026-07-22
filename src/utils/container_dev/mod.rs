//! Container Dev Mode: embedded OCI Distribution registry and engine-driver
//! dev loop for iterating on containers running on Avocado devices.
//!
//! Scaffolding at this stage. TLS material, the remaining registry listeners,
//! the engine-driver watcher, and sync orchestration are added by later tasks
//! in the `container-dev-mode` change.

// The write-side Basic validator (3.3) and the read/control Bearer validator
// (3.4).
#[allow(dead_code)]
pub mod auth;
pub mod config;
// The store (3.1), OCI read handlers (3.2), and write handlers + auth layer
// (3.3) land before the listeners that bind them: the read router is bound onto
// the dedicated bulk listener by 3.7, the write router onto the distinct write
// listener by 3.6/3.7.
#[allow(dead_code)]
pub mod registry;
#[allow(dead_code)]
pub mod store;
