//! Host mount table for the provision container.
//!
//! The stone provisioning scripts refuse to write a disk that is mounted. Inside
//! a container `/proc/self/mounts` shows the container's mounts, not the host's,
//! so a card the host has mounted looks free. The CLI takes a private copy of
//! the host table, binds it read-only into the container, and points
//! `AVOCADO_PROVISION_HOST_MOUNTS` at it. The library
//! (`avocado_safe_disk_unmounted`) reads that file as a `/proc/self/mounts`
//! table: first field is the source, and only sources under `/dev` matter.
//!
//! Sources such as `/dev/disk/by-uuid/...` are symlinks that the container
//! cannot resolve (its `/dev` differs), so they are rewritten to kernel names
//! (`/dev/sdb1`) before the copy is written.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Environment variable the provisioning library reads.
pub const ENV_VAR: &str = "AVOCADO_PROVISION_HOST_MOUNTS";

/// Fixed path the copy is bound to inside the container.
pub const CONTAINER_PATH: &str = "/run/avocado-provision/host-mounts";

/// Rewrite the source field of each mount table line using `resolve`.
///
/// Only the first whitespace-delimited field is touched; lines whose source the
/// resolver cannot map (or that are blank) pass through unchanged.
pub fn rewrite_mount_sources(table: &str, resolve: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(table.len());
    for line in table.lines() {
        match line.split_once(char::is_whitespace) {
            Some((src, rest)) if src.starts_with("/dev/") => {
                let new_src = resolve(src)
                    .map(|n| encode_mount_field(&n))
                    .unwrap_or_else(|| src.to_string());
                out.push_str(&new_src);
                out.push(' ');
                out.push_str(rest);
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

/// Decode the octal escapes `/proc/self/mounts` uses in its device field:
/// `\040` (space), `\011` (tab), `\012` (newline) and `\134` (backslash).
/// Anything else, including a malformed escape, is left as written.
fn decode_mount_escapes(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let decoded = match tail.get(1..4) {
            Some("040") => Some(' '),
            Some("011") => Some('\t'),
            Some("012") => Some('\n'),
            Some("134") => Some('\\'),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[4..];
            }
            None => {
                out.push('\\');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Inverse of [`decode_mount_escapes`], so a resolved name stays one field.
fn encode_mount_field(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            c => out.push(c),
        }
    }
    out
}

fn resolve_symlink(src: &str) -> Option<String> {
    std::fs::canonicalize(decode_mount_escapes(src))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// A private, owner-only copy of the host mount table. Dropping it removes the
/// file and its directory.
pub struct HostMountsCopy {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl HostMountsCopy {
    /// Write `table` to a new 0600 file in a fresh private temp directory.
    pub fn from_table(table: &str) -> std::io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("avocado-host-mounts-")
            .tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.path().join("mounts");
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&path)?;
        file.write_all(table.as_bytes())?;
        file.flush()?;
        Ok(Self { _dir: dir, path })
    }

    /// Copy the running host's mount table, symlink sources rewritten to kernel
    /// names. `None` off Linux or when the table cannot be read or written.
    #[cfg(target_os = "linux")]
    pub fn take() -> Option<Self> {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        Self::from_table(&rewrite_mount_sources(&table, &resolve_symlink)).ok()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn take() -> Option<Self> {
        None
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `-v` argument that binds the copy read-only at [`CONTAINER_PATH`].
    pub fn bind_arg(&self) -> String {
        format!("{}:{}:ro", self.path().display(), CONTAINER_PATH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_symlink_device_sources_to_kernel_names() {
        let table = "/dev/disk/by-uuid/abcd /media/card vfat rw 0 0\n\
                     /dev/mapper/vg-root / ext4 rw 0 0\n";
        let resolve = |s: &str| match s {
            "/dev/disk/by-uuid/abcd" => Some("/dev/sdb1".to_string()),
            "/dev/mapper/vg-root" => Some("/dev/dm-0".to_string()),
            _ => None,
        };
        assert_eq!(
            rewrite_mount_sources(table, &resolve),
            "/dev/sdb1 /media/card vfat rw 0 0\n/dev/dm-0 / ext4 rw 0 0\n"
        );
    }

    #[test]
    fn keeps_unresolvable_and_non_device_sources() {
        let table = "/dev/gone /mnt ext4 rw 0 0\ntmpfs /run tmpfs rw 0 0\nproc /proc proc rw 0 0\n";
        assert_eq!(rewrite_mount_sources(table, &|_| None), table);
    }

    #[test]
    fn rewrite_only_touches_the_source_field() {
        let table = "/dev/sda1 /dev/sda1 ext4 rw 0 0\n";
        let resolve = |_: &str| Some("/dev/X".to_string());
        assert_eq!(
            rewrite_mount_sources(table, &resolve),
            "/dev/X /dev/sda1 ext4 rw 0 0\n"
        );
    }

    #[test]
    fn decodes_mount_table_octal_escapes() {
        assert_eq!(
            decode_mount_escapes(r"/dev/disk/by-label/My\040Card"),
            "/dev/disk/by-label/My Card"
        );
        assert_eq!(decode_mount_escapes(r"a\011b"), "a\tb");
        assert_eq!(decode_mount_escapes(r"a\134b"), "a\\b");
        assert_eq!(decode_mount_escapes(r"a\012b"), "a\nb");
        assert_eq!(decode_mount_escapes("/dev/sda1"), "/dev/sda1");
    }

    #[test]
    fn leaves_invalid_escapes_alone() {
        assert_eq!(decode_mount_escapes(r"a\9zb"), r"a\9zb");
        assert_eq!(decode_mount_escapes(r"a\04"), r"a\04");
        assert_eq!(decode_mount_escapes(r"a\"), r"a\");
        assert_eq!(decode_mount_escapes(r"a\400b"), r"a\400b");
    }

    #[test]
    fn encodes_special_characters_back_to_a_valid_field() {
        assert_eq!(encode_mount_field("/dev/sdb1"), "/dev/sdb1");
        assert_eq!(encode_mount_field("/dev/a b"), r"/dev/a\040b");
        assert_eq!(encode_mount_field("/dev/a\tb"), r"/dev/a\011b");
    }

    #[cfg(unix)]
    #[test]
    fn resolves_an_escaped_by_label_source_to_the_kernel_name() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("sdb1");
        std::fs::write(&node, b"").unwrap();
        std::os::unix::fs::symlink(&node, dir.path().join("My Card")).unwrap();
        let escaped = format!("{}/My\\040Card", dir.path().display());
        let resolved = resolve_symlink(&escaped).expect("escaped source resolves");
        assert_eq!(
            resolved,
            std::fs::canonicalize(&node).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn unresolvable_escaped_source_keeps_the_original_text() {
        let table = "/dev/disk/by-label/My\\040Card /mnt vfat rw 0 0\n";
        assert_eq!(rewrite_mount_sources(table, &resolve_symlink), table);
    }

    #[test]
    fn resolved_name_with_a_space_is_written_escaped() {
        let table = "/dev/x /mnt vfat rw 0 0\n";
        let resolve = |_: &str| Some("/dev/a b".to_string());
        assert_eq!(
            rewrite_mount_sources(table, &resolve),
            "/dev/a\\040b /mnt vfat rw 0 0\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_is_owner_only_and_holds_the_table() {
        use std::os::unix::fs::PermissionsExt;
        let copy = HostMountsCopy::from_table("/dev/sda1 / ext4 rw 0 0\n").unwrap();
        let meta = std::fs::metadata(copy.path()).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let dir_meta = std::fs::metadata(copy.path().parent().unwrap()).unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::read_to_string(copy.path()).unwrap(),
            "/dev/sda1 / ext4 rw 0 0\n"
        );
    }

    #[test]
    fn copy_is_removed_on_drop() {
        let copy = HostMountsCopy::from_table("x\n").unwrap();
        let path = copy.path().to_path_buf();
        assert!(path.exists());
        drop(copy);
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn two_copies_get_distinct_paths() {
        let a = HostMountsCopy::from_table("a\n").unwrap();
        let b = HostMountsCopy::from_table("b\n").unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn bind_arg_is_read_only_at_the_fixed_container_path() {
        let copy = HostMountsCopy::from_table("a\n").unwrap();
        let arg = copy.bind_arg();
        assert_eq!(
            arg,
            format!(
                "{}:/run/avocado-provision/host-mounts:ro",
                copy.path().display()
            )
        );
        assert!(arg.ends_with(":ro"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn take_copies_the_live_mount_table() {
        let copy = HostMountsCopy::take().expect("linux has /proc/self/mounts");
        let body = std::fs::read_to_string(copy.path()).unwrap();
        assert!(body.lines().any(|l| l.contains(" / ")));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn take_is_none_off_linux() {
        assert!(HostMountsCopy::take().is_none());
    }
}
