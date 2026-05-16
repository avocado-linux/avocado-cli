//! `avocado vm rebuild` — replace the recorded manifest + artifacts with a
//! fresh `--vm-source`, optionally wiping the persistent data disk.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::utils::output::{print_info, print_success, OutputLevel};
use crate::utils::vm::manifest::Manifest;
use crate::utils::vm::state::VmPaths;

pub struct RebuildCommand {
    pub vm_source: Option<PathBuf>,
    pub reset_data: bool,
}

impl RebuildCommand {
    pub async fn execute(self) -> Result<()> {
        let vm_source = match self.vm_source {
            Some(p) => p,
            None => std::env::var("AVOCADO_VM_DIR")
                .map(PathBuf::from)
                .context("--vm-source not given and AVOCADO_VM_DIR is unset")?,
        };
        let paths = VmPaths::resolve()?;
        paths.ensure()?;

        // Refuse if the VM is still running — rebuilding under it would be
        // confusing. Caller should `avocado vm stop` first.
        if let Some(pid) = crate::utils::vm::state::read_pid(&paths)? {
            if crate::utils::vm::state::pid_alive(pid) {
                anyhow::bail!(
                    "avocado-vm is running (pid {pid}); run `avocado vm stop` before rebuild"
                );
            }
        }

        let manifest_src = vm_source.join("manifest.json");
        let manifest = Manifest::load(&manifest_src)
            .with_context(|| format!("load manifest at {}", manifest_src.display()))?;
        manifest
            .verify_all(&vm_source)
            .context("manifest sha256 verification")?;

        std::fs::copy(&manifest_src, paths.manifest())
            .with_context(|| format!("copy manifest to {}", paths.root.display()))?;
        print_info(
            &format!("Refreshed manifest -> {}", paths.manifest().display()),
            OutputLevel::Normal,
        );

        // Record artifact dir so Avocado.app can adopt it.
        let _ = std::fs::write(paths.artifact_dir_file(), vm_source.display().to_string());

        if self.reset_data {
            for (label, path) in [
                ("data.qcow2", paths.data_disk()),
                ("var.btrfs", paths.var_disk()),
            ] {
                if path.exists() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing {label}"))?;
                    print_info(
                        &format!("Removed {label} (will be re-seeded from the artifact on next start)."),
                        OutputLevel::Normal,
                    );
                }
            }
        }

        print_success("avocado-vm rebuilt.", OutputLevel::Normal);
        Ok(())
    }
}
