//! Integration tests for signing service and request handling

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn test_signing_request_serialization() {
        use serde_json;

        let request = serde_json::json!({
            "type": "sign_request",
            "binary_path": "/opt/_avocado/x86_64/runtimes/test/binary",
            "checksum_algorithm": "sha256"
        });

        let request_str = serde_json::to_string(&request).unwrap();
        assert!(request_str.contains("sign_request"));
        assert!(request_str.contains("binary"));
    }

    #[test]
    fn test_signing_response_serialization() {
        use serde_json;

        let response = serde_json::json!({
            "type": "sign_response",
            "success": true,
            "signature_path": "/opt/_avocado/x86_64/runtimes/test/binary.sig",
            "signature_content": "{}",
            "error": null
        });

        let response_str = serde_json::to_string(&response).unwrap();
        assert!(response_str.contains("sign_response"));
        assert!(response_str.contains("true"));
    }

    #[test]
    fn test_helper_script_contains_required_elements() {
        use avocado_cli::utils::signing_service::generate_helper_script;

        let script = generate_helper_script();

        // Check for required shebang
        assert!(script.starts_with("#!/bin/bash"));

        // Check for socket path
        assert!(script.contains("/run/avocado/sign.sock"));

        // Check for error handling
        assert!(script.contains("exit 1"));
        assert!(script.contains("exit 2"));
        assert!(script.contains("exit 3"));

        // Check for JSON request building
        assert!(script.contains("sign_request"));
        assert!(script.contains("binary_path"));
        assert!(script.contains("checksum_algorithm"));
    }

    #[test]
    fn test_run_config_with_signing_defaults() {
        use avocado_cli::utils::container::RunConfig;

        let config = RunConfig::default();

        assert!(config.signing_socket_path.is_none());
        assert!(config.signing_helper_script_path.is_none());
        assert!(config.signing_key_name.is_none());
        assert!(config.signing_checksum_algorithm.is_none());
    }

    #[test]
    fn test_run_config_with_signing_configured() {
        use avocado_cli::utils::container::RunConfig;

        let config = RunConfig {
            signing_socket_path: Some(PathBuf::from("/tmp/sign.sock")),
            signing_helper_script_path: Some(PathBuf::from("/tmp/helper.sh")),
            signing_key_name: Some("test-key".to_string()),
            signing_checksum_algorithm: Some("sha256".to_string()),
            ..Default::default()
        };

        assert!(config.signing_socket_path.is_some());
        assert!(config.signing_helper_script_path.is_some());
        assert_eq!(config.signing_key_name.unwrap(), "test-key");
        assert_eq!(config.signing_checksum_algorithm.unwrap(), "sha256");
    }
}

#[cfg(test)]
mod path_validation_tests {
    // Note: These tests are in a separate module because they test internal
    // functions that aren't publicly exposed. In the actual implementation,
    // the validation tests are in signing_request_handler.rs

    #[test]
    fn test_valid_binary_path() {
        // This would require exposing validate_binary_path or testing through
        // the public handle_signing_request function
        // For now, we rely on the unit tests in signing_request_handler.rs
    }
}
