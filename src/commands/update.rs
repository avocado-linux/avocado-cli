use anyhow::{Context, Result};
use std::path::Path;

use crate::utils::{
    config::Config,
    lockfile::LockFile,
    output::{print_info, print_success, OutputLevel},
    snapshot,
    target::resolve_target_required,
};

/// `avocado update` — move a target forward to the latest feed state.
///
/// Cargo-style: re-resolves the lock against the newest published snapshot.
/// Concretely it (1) advances the target's snapshot pin to the channel's
/// current `latest` snapshot, and (2) clears the package + kernel version pins
/// so the next `avocado install`/`fetch` re-selects the latest versions within
/// that new snapshot and re-locks them.
///
/// Everyday `install`/`fetch` stay reproducible (they reuse the pins); this is
/// the deliberate, explicit "move forward" action.
pub struct UpdateCommand {
    config_path: String,
    target: Option<String>,
    verbose: bool,
}

impl UpdateCommand {
    pub fn new(config_path: String, target: Option<String>, verbose: bool) -> Self {
        Self {
            config_path,
            target,
            verbose,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        let config = Config::load(&self.config_path)
            .with_context(|| format!("Failed to load config from {}", self.config_path))?;
        let target = resolve_target_required(self.target.as_deref(), &config)?;

        let src_dir = config
            .get_resolved_src_dir(&self.config_path)
            .unwrap_or_else(|| {
                Path::new(&self.config_path)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_path_buf()
            });

        let mut lock_file = LockFile::load(&src_dir)
            .with_context(|| format!("Failed to load lock file from {}", src_dir.display()))?;
        let old_snapshot = lock_file
            .get_repo_snapshot(&target)
            .map(|s| s.snapshot.clone());

        // Resolve the channel's current latest snapshot (no env/lock side effects).
        let latest = snapshot::resolve_latest(&config, &target).await?;

        // Re-resolve packages to latest by dropping the existing package +
        // kernel pins (and the old snapshot pin); the next build re-selects and
        // re-locks within the new snapshot.
        lock_file.clear_all(&target);

        match latest {
            Some(new_pin) => {
                let new_id = new_pin.snapshot.clone();
                let feed = format!("{}/{}", new_pin.release, new_pin.channel);
                lock_file.set_repo_snapshot(&target, new_pin);
                lock_file
                    .save_replacing(&src_dir)
                    .with_context(|| "Failed to save lock file")?;

                match old_snapshot {
                    Some(old) if old == new_id => print_info(
                        &format!("Already on the latest {feed} snapshot '{new_id}'."),
                        OutputLevel::Normal,
                    ),
                    Some(old) => print_info(
                        &format!("Advanced {feed} snapshot '{old}' -> '{new_id}' for '{target}'."),
                        OutputLevel::Normal,
                    ),
                    None => print_info(
                        &format!("Pinned {feed} to latest snapshot '{new_id}' for '{target}'."),
                        OutputLevel::Normal,
                    ),
                }
                print_success(
                    &format!(
                        "Updated '{target}'. Run 'avocado install' to resolve and lock the latest \
                         package versions within snapshot '{new_id}'."
                    ),
                    OutputLevel::Normal,
                );
            }
            None => {
                // No snapshot to advance to (feed serves no snapshots, or
                // releasever is manually overridden). Still honor the
                // "move to latest" intent for packages: cleared pins mean the
                // next build resolves the latest available head.
                lock_file
                    .save_replacing(&src_dir)
                    .with_context(|| "Failed to save lock file")?;
                if self.verbose {
                    print_info(
                        "Feed serves no snapshots (or releasever is overridden); no snapshot pin to advance.",
                        OutputLevel::Normal,
                    );
                }
                print_success(
                    &format!(
                        "Cleared package pins for '{target}'. Run 'avocado install' to resolve and \
                         lock the latest available versions."
                    ),
                    OutputLevel::Normal,
                );
            }
        }

        Ok(())
    }
}
