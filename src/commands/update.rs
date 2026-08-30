use anyhow::{Context, Result};
use std::path::Path;

use crate::utils::{
    config::Config,
    container::{RunConfig, SdkContainer},
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

        // The SDK keeps dnf's repodata cache on the persistent volume, so a feed
        // whose contents were replaced under the same URL (a dev repo, or a
        // channel head between snapshots) stays invisible until the metadata
        // expires - days. "Move forward" has to include forgetting what the
        // feed used to say; the next install re-reads it. Best-effort: a
        // missing SDK image or container is reported, not fatal.
        self.refresh_sdk_metadata(&config, &target).await;

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

    async fn refresh_sdk_metadata(&self, config: &Config, target: &str) {
        let Some(container_image) = config.get_sdk_image() else {
            if self.verbose {
                print_info(
                    "No SDK image configured; not refreshing the SDK's dnf metadata cache.",
                    OutputLevel::Normal,
                );
            }
            return;
        };
        let helper = match SdkContainer::from_config(&self.config_path, config) {
            Ok(h) => h.verbose(self.verbose),
            Err(e) => {
                print_info(
                    &format!("Not refreshing the SDK's dnf metadata cache: {e}"),
                    OutputLevel::Normal,
                );
                return;
            }
        };
        let run = RunConfig {
            container_image: container_image.to_string(),
            target: target.to_string(),
            command: metadata_refresh_script().to_string(),
            verbose: self.verbose,
            // The entrypoint defines DNF_SDK_HOST & co. natively; sourcing the
            // environment file instead mangles those multi-line exports (the
            // sysroot installs run the same way).
            source_environment: false,
            interactive: false,
            ..Default::default()
        };
        match helper.run_in_container(run).await {
            Ok(true) => {
                if self.verbose {
                    print_info("Expired the SDK's dnf metadata cache.", OutputLevel::Normal);
                }
            }
            Ok(false) => print_info(
                "Could not expire the SDK's dnf metadata cache; the next install may still see the previous feed contents.",
                OutputLevel::Normal,
            ),
            Err(e) => print_info(
                &format!("Could not expire the SDK's dnf metadata cache ({e}); the next install may still see the previous feed contents."),
                OutputLevel::Normal,
            ),
        }
    }
}

/// Expire dnf's cached repodata for both repo sets the SDK uses (host tools and
/// target sysroots). `clean expire-cache` keeps the downloaded packages and only
/// marks metadata stale, so the next dnf run re-fetches repomd and nothing else.
fn metadata_refresh_script() -> &'static str {
    r#"
# The sysroot install stamps record "these inputs produced this sysroot"; a
# feed that changed under them is not an input they can see, so a stamp that
# still reads current would make the next install skip dnf altogether. Moving
# forward means the next install has to run - drop them (same as
# `avocado clean --stamps`).
if [ -d "$AVOCADO_PREFIX/.stamps" ]; then
    rm -rf "$AVOCADO_PREFIX/.stamps"
fi
# dnf keeps every repo's metadata under $DNF_SDK_HOST_PREFIX/var/cache/<repo>-<hash>/repodata
# (host and target repos alike). Removing just the repodata directories is
# what `dnf clean expire-cache` achieves without depending on how the dnf
# wrapper composes its arguments: the next dnf run sees no metadata and
# fetches repomd again; downloaded packages stay.
if [ -d "$DNF_SDK_HOST_PREFIX/var/cache" ]; then
    find "$DNF_SDK_HOST_PREFIX/var/cache" -mindepth 2 -maxdepth 2 -name repodata -type d -exec rm -rf {} +
fi
"#
}

#[cfg(test)]
mod tests {
    use super::metadata_refresh_script;

    #[test]
    fn update_drops_cached_repodata_and_stamps_without_dropping_packages() {
        let s = metadata_refresh_script();
        assert!(
            s.contains("-name repodata -type d -exec rm -rf {} +"),
            "cached repodata is removed under the SDK dnf cache: {s}"
        );
        assert!(
            s.contains("$DNF_SDK_HOST_PREFIX/var/cache"),
            "the shared host/target cache dir: {s}"
        );
        assert!(
            !s.contains("clean all"),
            "packages stay cached; only metadata expires"
        );
        assert!(
            s.contains("rm -rf \"$AVOCADO_PREFIX/.stamps\""),
            "install stamps are dropped so the next install cannot skip dnf: {s}"
        );
    }
}
