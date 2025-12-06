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
    let config_path = std::env::current_dir()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("configs")
        .join("with-confext.yaml");
    let result =
        common::cli_with_config(&["ext", "build", "test-confext"], None, Some(&config_path));
    // Should complete regardless of Docker availability
    common::assert_command_completes(&result);
}
