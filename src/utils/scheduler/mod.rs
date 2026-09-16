//! Task scheduler with DAG-based dependency resolution and bounded parallelism.

pub mod dag;
pub mod executor;

pub use dag::TaskGraph;
pub use executor::TaskScheduler;

/// How many tasks run at once. The install and build DAGs and the SDK
/// phase's fixed four (sdk packages, rootfs, initramfs, target-dev) all read
/// their parallelism from here, so the rule cannot drift between them.
///
/// `AVOCADO_PARALLEL_TASKS` overrides; the default is the CPU count capped
/// at 4. Remote execution (`--runs-on`) is sequential.
pub fn max_parallel(remote: bool) -> usize {
    if remote {
        return 1;
    }
    std::env::var("AVOCADO_PARALLEL_TASKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| num_cpus::get().min(4))
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_is_sequential() {
        assert_eq!(max_parallel(true), 1);
    }

    #[test]
    #[serial_test::serial]
    fn env_override_wins_and_is_clamped() {
        std::env::set_var("AVOCADO_PARALLEL_TASKS", "3");
        assert_eq!(max_parallel(false), 3);
        std::env::set_var("AVOCADO_PARALLEL_TASKS", "0");
        assert_eq!(max_parallel(false), 1);
        std::env::remove_var("AVOCADO_PARALLEL_TASKS");
        assert!((1..=4).contains(&max_parallel(false)));
    }

    /// The SDK phase ran its four installs one at a time whenever `--force`
    /// was off, because it kept a private copy of the parallelism rule and
    /// the install DAG had moved on without it. Every site reads it here.
    #[test]
    fn every_site_reads_the_shared_rule() {
        for (name, src) in [
            ("install.rs", include_str!("../../commands/install.rs")),
            ("build.rs", include_str!("../../commands/build.rs")),
            (
                "sdk/install.rs",
                include_str!("../../commands/sdk/install.rs"),
            ),
        ] {
            assert!(
                src.contains("scheduler::max_parallel("),
                "{name} does not call scheduler::max_parallel"
            );
            assert!(
                !src.contains(r#"env::var("AVOCADO_PARALLEL_TASKS")"#),
                "{name} re-reads AVOCADO_PARALLEL_TASKS itself"
            );
        }
    }
}
