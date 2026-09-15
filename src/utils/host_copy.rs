//! Copy files from the SDK docker volume out to the host filesystem.
//!
//! The SDK volume (e.g. `avocado-<target>`) is mounted at `/opt/_avocado`
//! inside the container; anything written under `$AVOCADO_PREFIX/...`
//! during a build survives container exit. To make those files visible on the
//! host we `docker cp` out of a container that has the volume mounted.
//!
//! That container is the session container, not a fresh one: `docker cp` works
//! against a running container, so a copy costs one `cp` instead of the
//! `create` + `cp` + `rm` — on a third-party `busybox` image — that it used to.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::process::Command;

/// Copy `<container_path>` from the SDK volume to `<host_path>`. Creates the
/// host path's parent if missing.
///
/// `image` is the project's SDK image. It is only used if this is the first
/// caller to need a container for this volume; after that the existing one is
/// reused and the image is irrelevant.
pub async fn copy_volume_path_to_host(
    container_tool: &str,
    volume_name: &str,
    image: &str,
    container_path: &str,
    host_path: &Path,
) -> Result<()> {
    if let Some(parent) = host_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("copy_volume_path_to_host: mkdir -p {}", parent.display()))?;
    }
    let cid = crate::utils::container::SessionContainers::volume_container(
        container_tool,
        &format!("{volume_name}:/opt/_avocado:ro"),
        image,
    )?;
    let result = Command::new(container_tool)
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
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        anyhow::bail!("docker cp failed: {stderr}");
    }
    Ok(())
}
