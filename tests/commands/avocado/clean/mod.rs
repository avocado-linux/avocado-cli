use crate::common;
use tempfile::TempDir;

#[test]
fn test_long_help() {
    common::assert_cmd(&["clean", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["clean", "-h"], None, None);
}

/// In a project of its own: run in the repository, `clean` removed the checkout's own
/// build volume, and failed whenever another test had a container on it.
#[test]
fn test_clean_command() {
    let dir = TempDir::new().unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/avocado.yaml"),
        dir.path().join("avocado.yaml"),
    )
    .unwrap();
    common::assert_cmd(&["clean", dir.path().to_str().unwrap()], None, None);
}
