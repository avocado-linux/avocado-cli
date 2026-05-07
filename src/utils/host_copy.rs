//! Copy files from the SDK docker volume out to the host filesystem.
//!
//! The SDK volume (e.g. `avocado-<target>`) is mounted at `/opt/_avocado`
//! inside the container; anything written under `$AVOCADO_PREFIX/...`
//! during a build survives container exit. To make those files visible
//! on the host we spin up a one-shot busybox container that mounts the
//! volume read-only and `docker cp` the file out.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

async fn create_temp_container(volume_name: &str) -> Result<String> {
    let output = Command::new("docker")
        .args([
            "create",
            "--rm",
            "-v",
            &format!("{volume_name}:/opt/_avocado"),
            "busybox",
            "true",
        ])
        .output()
        .await
        .context("Failed to create temp container for volume cp")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to create temp container: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Copy `<container_path>` from the SDK volume to `<host_path>`. Creates
/// the host path's parent if missing. The temp container is always
/// `docker rm -f`'d, even on failure.
pub async fn copy_volume_path_to_host(
    volume_name: &str,
    container_path: &str,
    host_path: &Path,
) -> Result<()> {
    if let Some(parent) = host_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("copy_volume_path_to_host: mkdir -p {}", parent.display()))?;
    }
    let cid = create_temp_container(volume_name).await?;
    let result = Command::new("docker")
        .args([
            "cp",
            &format!("{cid}:{container_path}"),
            host_path
                .to_str()
                .context("host destination path is not valid UTF-8")?,
        ])
        .output()
        .await
        .context("Failed to run docker cp")?;
    let _ = Command::new("docker")
        .args(["rm", "-f", &cid])
        .output()
        .await;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("docker cp failed: {stderr}");
    }
    Ok(())
}
