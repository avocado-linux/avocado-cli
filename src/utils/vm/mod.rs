//! `avocado-vm` host-side support.
//!
//! On macOS dev hosts (and eventually Windows), avocado-cli runs a single
//! user-level QEMU guest that hosts its own `dockerd` and exposes a 9p
//! source mount. The CLI owns the qemu process directly (spawn, signal,
//! pidfile); Avocado.app, when installed, observes by adopting whatever
//! pidfile shows. USB passthrough is handled out-of-band by Avocado.app's
//! IOUSBHost → USB/IP → vhci_hcd bridge (see avocado-vm/macos/). This
//! module manages the VM's lifecycle and the control channels avocado-cli
//! uses to drive it (QMP for QEMU-level ops, qga for guest-side ops
//! without SSH, and SSH for everything else).
//!
//! Public submodules:
//! - [`state`]   — `~/.avocado/vm/` directory layout + paths.
//! - [`manifest`] — parses + sha256-verifies the `direct` profile output.
//! - [`qmp`]     — QEMU monitor JSON-RPC client.
//! - [`qga`]     — qemu-guest-agent JSON-RPC client.
//! - [`ssh`]     — thin ssh wrapper for the VM target.
//! - [`qemu`]    — assembles + spawns the `qemu-system-*` process.
//! - [`boot_sync`] — boot handshake (qga `guest-sync` polling).
//! - [`lifecycle`] — high-level `start` / `stop` / `status`.

pub mod boot_sync;
pub mod channel;
#[cfg(target_os = "macos")]
pub mod client;
pub mod config;
pub mod fdt;
pub mod forward;
pub mod lifecycle;
pub mod manifest;
pub mod qemu;
#[cfg(unix)]
pub mod qga;
#[cfg(unix)]
pub mod qmp;
pub mod route;
pub mod share;
pub mod ssh;
pub mod staging;
pub mod state;
pub mod supervisor;
