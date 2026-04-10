use anyhow::{Context, Result};
use std::path::Path;

use crate::utils::config_edit;
use crate::utils::output::{print_info, print_success, print_warning, OutputLevel};

pub struct ConnectCleanCommand {
    pub runtime: String,
    pub config_path: String,
}

impl ConnectCleanCommand {
    pub fn execute(&self) -> Result<()> {
        let config_path = Path::new(&self.config_path);
        if !config_path.exists() {
            anyhow::bail!(
                "Config file '{}' not found. Run this command from your project directory.",
                self.config_path
            );
        }

        let config_dir = config_path.parent().unwrap_or(Path::new("."));
        let mut any_changes = false;

        // 1. Remove connect: section from avocado.yaml
        match config_edit::remove_connect_fields(config_path) {
            Ok(true) => {
                print_success("Removed connect section from avocado.yaml.", OutputLevel::Normal);
                any_changes = true;
            }
            Ok(false) => {
                print_info(
                    "No connect section found in avocado.yaml.",
                    OutputLevel::Normal,
                );
            }
            Err(e) => {
                print_warning(
                    &format!("Failed to remove connect section: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        // 2. Remove avocado-ext-connect-config extension from avocado.yaml
        match config_edit::remove_connect_config_extension(config_path, &self.runtime) {
            Ok(true) => {
                print_success(
                    "Removed avocado-ext-connect-config extension from avocado.yaml.",
                    OutputLevel::Normal,
                );
                any_changes = true;
            }
            Ok(false) => {
                print_info(
                    "No avocado-ext-connect-config extension found in avocado.yaml.",
                    OutputLevel::Normal,
                );
            }
            Err(e) => {
                print_warning(
                    &format!("Failed to remove connect-config extension: {e}"),
                    OutputLevel::Normal,
                );
            }
        }

        // 3. Remove overlay/etc/avocado-conn/ directory (contains config.toml)
        let overlay_conn_dir = config_dir.join("overlay/etc/avocado-conn");
        if overlay_conn_dir.exists() {
            std::fs::remove_dir_all(&overlay_conn_dir)
                .with_context(|| format!("Failed to remove {}", overlay_conn_dir.display()))?;
            print_success(
                &format!("Removed {}", overlay_conn_dir.display()),
                OutputLevel::Normal,
            );
            any_changes = true;
        } else {
            print_info(
                &format!("{} does not exist, skipping.", overlay_conn_dir.display()),
                OutputLevel::Normal,
            );
        }

        if any_changes {
            println!();
            print_success("Connect configuration cleaned.", OutputLevel::Normal);
        } else {
            println!();
            print_info(
                "Nothing to clean — no connect configuration found.",
                OutputLevel::Normal,
            );
        }

        Ok(())
    }
}
