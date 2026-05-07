//! Kernel commands. The kernel binary itself ships via the rootfs
//! sysroot's `kernel-image-*` package — installation is handled by
//! `avocado rootfs install`. This module provides the standalone
//! `avocado kernel image` subcommand that wraps that binary into a
//! signed `kos.layer.kernel` KAB.

pub mod image;

pub use image::KernelImageCommand;
