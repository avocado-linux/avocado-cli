//! `docker cp`, over the archive endpoints.

use super::{parse, USE_CLI_ENV};
use anyhow::{bail, Context, Result};
use bollard::query_parameters as q;
use bollard::Docker;
use futures_util::{StreamExt, TryStreamExt};
use std::path::Path;
use tokio_util::io::{StreamReader, SyncIoBridge};

/// Go's `os.ModeDir`, the form the daemon reports a path's mode in.
const MODE_DIR: u32 = 1 << 31;

pub(super) async fn cp(docker: &Docker, args: &[String]) -> Result<i32> {
    let a = parse(args, &[], &["-q|--quiet"], true)?;
    let [src, dst] = a.rest.as_slice() else {
        bail!("docker cp: expected a source and a destination");
    };
    match (container_path(src), container_path(dst)) {
        (Some((container, path)), None) => {
            from_container(docker, container, path, Path::new(dst)).await?
        }
        (None, Some((container, path))) => {
            to_container(docker, Path::new(src), container, path).await?
        }
        _ => bail!("docker cp: one of the source and the destination must be CONTAINER:PATH"),
    }
    Ok(0)
}

/// Split `CONTAINER:PATH`. Anything absolute or starting with `.` is a local
/// path, colon or not, as the docker CLI has it.
fn container_path(arg: &str) -> Option<(&str, &str)> {
    if Path::new(arg).is_absolute() {
        return None;
    }
    arg.split_once(':')
        .filter(|(container, _)| !container.starts_with('.'))
}

async fn from_container(docker: &Docker, container: &str, path: &str, dst: &Path) -> Result<()> {
    let options = q::DownloadFromContainerOptionsBuilder::new()
        .path(path)
        .build();
    let mut archive = Box::pin(docker.download_from_container(container, Some(options)));
    // Take the first chunk here: a path that is not there fails on it, and
    // this way with the daemon's own message rather than a tar error.
    let first = archive.try_next().await?.unwrap_or_default();
    let archive = futures_util::stream::iter([Ok(first)]).chain(archive);

    // The archive is one tree, rooted at the last component of `path`. Into
    // an existing directory it unpacks as it is. Under any other destination
    // it takes the destination's name, so unpack it next door and rename.
    let into_dir = dst.is_dir();
    if into_dir && path.ends_with("/.") {
        bail!(
            "docker cp: merging a directory's contents (SRC/.) into an existing directory is \
             unsupported (set {USE_CLI_ENV}=1 to use the docker CLI)"
        );
    }
    let staging = if into_dir {
        None
    } else {
        let parent = dst.parent().filter(|p| !p.as_os_str().is_empty());
        let staging = tempfile::Builder::new()
            .prefix(".avocado-cp-")
            .tempdir_in(parent.unwrap_or(Path::new(".")))
            .with_context(|| format!("docker cp: cannot write next to {}", dst.display()))?;
        Some(staging)
    };
    let unpack_dir = staging.as_ref().map_or(dst, |s| s.path()).to_path_buf();

    let archive = Box::pin(archive.map_err(std::io::Error::other));
    let reader = SyncIoBridge::new(StreamReader::new(archive));
    tokio::task::spawn_blocking(move || {
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.unpack(unpack_dir)
    })
    .await?
    .with_context(|| format!("docker cp: cannot unpack {container}:{path}"))?;

    if let Some(staging) = staging {
        let root = std::fs::read_dir(staging.path())?
            .next()
            .context("docker cp: the daemon sent an empty archive")??
            .path();
        rename(&root, dst).with_context(|| format!("docker cp: cannot write {}", dst.display()))?;
    }
    Ok(())
}

fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
    // Moving a directory to a new parent rewrites its `..`, which takes write
    // permission on the directory itself, and a copied one may not grant it.
    #[cfg(unix)]
    if let Some(mode) = std::fs::symlink_metadata(from)
        .ok()
        .filter(|meta| meta.is_dir())
        .map(|meta| meta.permissions())
    {
        use std::os::unix::fs::PermissionsExt;
        let writable = std::fs::Permissions::from_mode(mode.mode() | 0o200);
        std::fs::set_permissions(from, writable)?;
        std::fs::rename(from, to)?;
        return std::fs::set_permissions(to, mode);
    }
    std::fs::rename(from, to)
}

async fn to_container(docker: &Docker, src: &Path, container: &str, path: &str) -> Result<()> {
    // Into an existing directory the source keeps its name; otherwise the
    // destination names it, inside the destination's parent.
    let stat = q::ContainerArchiveInfoOptionsBuilder::new()
        .path(path)
        .build();
    let is_dir = docker
        .get_container_archive_info(container, Some(stat))
        .await
        .is_ok_and(|stat| stat.file_mode & MODE_DIR != 0);
    let (dir, name) = if is_dir {
        let name = src.file_name().context("docker cp: source has no name")?;
        (path, name.to_string_lossy().into_owned())
    } else {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        (if dir.is_empty() { "/" } else { dir }, name.to_string())
    };

    // ponytail: the archive is built in memory. Fine for the signature files
    // this is used for; stream it if something large ever goes this way.
    let mut archive = tar::Builder::new(Vec::new());
    archive.follow_symlinks(false);
    if src.is_dir() {
        archive.append_dir_all(&name, src)
    } else {
        archive.append_path_with_name(src, &name)
    }
    .with_context(|| format!("docker cp: cannot read {}", src.display()))?;
    let archive = archive.into_inner()?;

    let options = q::UploadToContainerOptionsBuilder::new().path(dir).build();
    docker
        .upload_to_container(container, Some(options), bollard::body_full(archive.into()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_paths_are_told_from_local_ones() {
        assert_eq!(container_path("abc123:/opt/x"), Some(("abc123", "/opt/x")));
        assert_eq!(container_path("/tmp/a:b"), None);
        assert_eq!(container_path("./a:b"), None);
        assert_eq!(container_path("plain"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_can_still_be_moved_into_place() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("staging/tree");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("file"), "x").unwrap();
        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o555)).unwrap();

        let to = tmp.path().join("out");
        rename(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(to.join("file")).unwrap(), "x");
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o555);
        // Let the tempdir clean up.
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
