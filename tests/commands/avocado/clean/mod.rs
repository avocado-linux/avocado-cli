use crate::common;

#[test]
fn test_long_help() {
    common::assert_cmd(&["clean", "--help"], None, None);
}

#[test]
fn test_short_help() {
    common::assert_cmd(&["clean", "-h"], None, None);
}

#[test]
fn test_clean_command() {
    common::assert_cmd(&["clean"], None, None);
}
