//! Container Dev Mode: embedded OCI Distribution registry and engine-driver
//! dev loop for iterating on containers running on Avocado devices.
//!
//! Scaffolding only at this stage. TLS material, the remaining registry
//! listeners, the engine-driver watcher, and sync orchestration are added by
//! later tasks in the `container-dev-mode` change.

pub mod config;
// Write handlers, GC, and sync orchestration land in later `container-dev-mode`
// tasks (3.3-3.5, 5.x); the store (3.1) and the OCI read handlers (3.2) land
// first. The read router is bound onto the dedicated bulk listener by task 3.7.
#[allow(dead_code)]
pub mod registry;
#[allow(dead_code)]
pub mod store;
