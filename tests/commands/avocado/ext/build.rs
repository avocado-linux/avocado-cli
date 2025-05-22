use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["ext", "build", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["ext", "build", "-h"], None, None);
}

#[test]
fn test_ext_build_missing_extension() {
    common::refute_cmd(&["ext", "build", "nonexistent"], None, None);
}

#[test]
fn test_ext_build_with_fixture_extension() {
    common::assert_cmd(&["ext", "build", "test-confext"], None, None);
}
