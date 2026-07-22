//! Container Dev Mode: embedded OCI Distribution registry and engine-driver
//! dev loop for iterating on containers running on Avocado devices.
//!
//! Scaffolding only at this stage. TLS material, the registry listeners, the
//! engine-driver watcher, and sync orchestration are added by later tasks in
//! the `container-dev-mode` change.

pub mod config;
// Consumers (OCI read/write handlers, GC, sync orchestration) land in later
// `container-dev-mode` tasks (3.2-3.5, 5.x); the store lands first.
#[allow(dead_code)]
pub mod store;
