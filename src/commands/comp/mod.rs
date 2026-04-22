//! Component commands — `avocado comp <subcommand>`.
//!
//! Components are top-level `components.<name>` entries in avocado.yaml
//! (see also client-kondra/docs/kernel_amf.md). They describe pieces of the
//! OS that aren't runtime extensions — base rootfs, initramfs, kernel — and
//! are signed into the AMF alongside extensions.
//!
//! The `comp` subcommands mirror `ext` (install/build/image/clean/list) so
//! a component's KAB can be produced in standalone mode, without running a
//! full runtime build.

pub mod image;
pub mod list;

pub use image::CompImageCommand;
pub use list::CompListCommand;
