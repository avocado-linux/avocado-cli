//! `avocado vm` subcommands.
//!
//! Thin clap/dispatch layer over [`crate::utils::vm::lifecycle`]. The actual
//! work lives in the utils submodules; these modules just present the user-
//! facing flags and pretty-print results.

pub mod config;
pub mod logs;
pub mod rebuild;
pub mod reset;
pub mod shell;
pub mod start;
pub mod status;
pub mod stop;
#[cfg(unix)]
pub mod supervise;
pub mod update;

/// Printed after `vm reset` and `vm rebuild --reset-data`, which delete the
/// VM's docker volumes. A project's `.avocado-state` still names its volume,
/// so its next command refuses until `avocado clean` drops that state.
pub const BUILD_VOLUMES_DELETED_NOTE: &str =
    "Build volumes were deleted: run `avocado clean` in each project built before, ahead of its next build.";

#[cfg(test)]
mod tests {
    use super::BUILD_VOLUMES_DELETED_NOTE;

    #[test]
    fn data_reset_note_points_projects_at_avocado_clean() {
        assert!(BUILD_VOLUMES_DELETED_NOTE.contains("`avocado clean`"));
        assert!(!BUILD_VOLUMES_DELETED_NOTE.contains('\n'));
    }
}
