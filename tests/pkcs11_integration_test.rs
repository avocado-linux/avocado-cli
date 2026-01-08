//! Integration tests for PKCS#11 device support
//!
//! These tests automatically start a software TPM (swtpm) for testing.
//! Prerequisites:
//!
//! 1. Install swtpm and libtpm2-pkcs11-1:
//!    ```bash
//!    sudo apt-get install swtpm swtpm-tools libtpm2-pkcs11-1 tpm2-tools
//!    ```
//!
//! 2. Run the tests:
//!    ```bash
//!    cargo test --test pkcs11_integration_test
//!    ```
//!
//! The tests will automatically:
//! - Start a software TPM simulator
//! - Initialize it
//! - Set up the environment
//! - Run tests
//! - Clean up

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

/// SWTPM manager to handle TPM simulator lifecycle
struct SwtpmInstance {
    process: Option<Child>,
    state_dir: PathBuf,
}

impl SwtpmInstance {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Create temporary directory for TPM state
        let state_dir = std::env::temp_dir().join(format!("tpm-test-{}", std::process::id()));
        fs::create_dir_all(&state_dir)?;

        let port = 2321;

        // Start swtpm socket
        let process = Command::new("swtpm")
            .args([
                "socket",
                "--tpmstate",
                &format!("dir={}", state_dir.display()),
                "--tpm2",
                "--ctrl",
                &format!("type=tcp,port={}", port + 1),
                "--server",
                &format!("type=tcp,port={port}"),
                "--flags",
                "not-need-init",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        // Wait for swtpm to be ready
        thread::sleep(Duration::from_millis(500));

        // Set environment variables for TPM communication
        env::set_var(
            "TPM2TOOLS_TCTI",
            format!("swtpm:host=127.0.0.1,port={port}"),
        );
        env::set_var(
            "TPM2_PKCS11_TCTI",
            format!("swtpm:host=127.0.0.1,port={port}"),
        );

        let instance = SwtpmInstance {
            process: Some(process),
            state_dir,
        };

        // Initialize the TPM
        instance.initialize_tpm()?;

        Ok(instance)
    }

    fn initialize_tpm(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Initialize TPM
        let output = Command::new("tpm2_startup").arg("-c").output()?;

        if !output.status.success() {
            eprintln!(
                "Failed to initialize TPM: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Create primary key
        let output = Command::new("tpm2_createprimary")
            .args([
                "-C",
                "o",
                "-c",
                &format!("{}/primary.ctx", self.state_dir.display()),
            ])
            .output()?;

        if !output.status.success() {
            eprintln!(
                "Failed to create primary key: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    fn is_available() -> bool {
        // Check if swtpm is installed
        Command::new("swtpm")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl Drop for SwtpmInstance {
    fn drop(&mut self) {
        // Kill swtpm process
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }

        // Clean up state directory
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

/// Check if TPM PKCS#11 library is available
fn tpm_pkcs11_available() -> bool {
    use avocado_cli::utils::pkcs11_devices::{get_pkcs11_module_path, DeviceType};

    get_pkcs11_module_path(&DeviceType::Tpm).is_ok()
}

#[test]
#[ignore] // Run with: cargo test --test pkcs11_integration_test -- --ignored
fn test_pkcs11_uri_parsing() {
    use avocado_cli::utils::pkcs11_devices::parse_pkcs11_uri;

    // Test valid URIs
    let (token, object) = parse_pkcs11_uri("pkcs11:token=MyToken;object=signing-key").unwrap();
    assert_eq!(token, "MyToken");
    assert_eq!(object, "signing-key");

    // Test URI with spaces (encoded)
    let (token, object) =
        parse_pkcs11_uri("pkcs11:token=Test%20Token;object=my%20key;type=private").unwrap();
    assert_eq!(token, "Test Token");
    assert_eq!(object, "my key");

    // Test invalid URIs
    assert!(parse_pkcs11_uri("invalid:token=Test").is_err());
    assert!(parse_pkcs11_uri("pkcs11:token=Test").is_err()); // Missing object
    assert!(parse_pkcs11_uri("pkcs11:object=test").is_err()); // Missing token
}

#[test]
#[ignore] // Run with: cargo test --test pkcs11_integration_test -- --ignored
fn test_pkcs11_uri_building() {
    use avocado_cli::utils::pkcs11_devices::build_pkcs11_uri;

    let uri = build_pkcs11_uri("MyToken", "signing-key");
    assert_eq!(uri, "pkcs11:token=MyToken;object=signing-key;type=private");

    // Test with spaces
    let uri = build_pkcs11_uri("Test Token", "my key");
    assert!(uri.contains("Test%20Token"));
    assert!(uri.contains("my%20key"));
}

#[test]
#[ignore] // Run with: cargo test --test pkcs11_integration_test -- --ignored
fn test_device_type_parsing() {
    use avocado_cli::utils::pkcs11_devices::DeviceType;
    use std::str::FromStr;

    assert_eq!(DeviceType::from_str("tpm").unwrap(), DeviceType::Tpm);
    assert_eq!(DeviceType::from_str("TPM").unwrap(), DeviceType::Tpm);
    assert_eq!(
        DeviceType::from_str("yubikey").unwrap(),
        DeviceType::Yubikey
    );
    assert_eq!(DeviceType::from_str("yk").unwrap(), DeviceType::Yubikey);
    assert_eq!(DeviceType::from_str("auto").unwrap(), DeviceType::Auto);
    assert!(DeviceType::from_str("invalid").is_err());
}

#[test]
#[ignore] // Run with: cargo test --test pkcs11_integration_test -- --ignored
fn test_auth_method_parsing() {
    use avocado_cli::utils::pkcs11_devices::Pkcs11AuthMethod;
    use std::str::FromStr;

    match Pkcs11AuthMethod::from_str("none").unwrap() {
        Pkcs11AuthMethod::None => {}
        _ => panic!("Expected None"),
    }

    match Pkcs11AuthMethod::from_str("prompt").unwrap() {
        Pkcs11AuthMethod::Prompt => {}
        _ => panic!("Expected Prompt"),
    }

    match Pkcs11AuthMethod::from_str("env").unwrap() {
        Pkcs11AuthMethod::EnvVar(v) => assert_eq!(v, "AVOCADO_PKCS11_PIN"),
        _ => panic!("Expected EnvVar"),
    }

    assert!(Pkcs11AuthMethod::from_str("invalid").is_err());
}

#[test]
#[ignore] // Run with: cargo test --test pkcs11_integration_test -- --ignored
fn test_keyid_generation() {
    use avocado_cli::utils::pkcs11_devices::generate_keyid_from_public_key;

    let test_key = b"test public key data";
    let keyid = generate_keyid_from_public_key(test_key);

    assert!(keyid.starts_with("sha256-"));
    assert_eq!(keyid.len(), 7 + 16); // "sha256-" + 16 hex chars

    // Test determinism
    let keyid2 = generate_keyid_from_public_key(test_key);
    assert_eq!(keyid, keyid2);
}

#[test]
fn test_tpm_connection() {
    if !SwtpmInstance::is_available() {
        eprintln!("swtpm not available, skipping test. Install: sudo apt install swtpm");
        return;
    }

    if !tpm_pkcs11_available() {
        eprintln!("TPM PKCS#11 library not available, skipping test. Install: sudo apt install libtpm2-pkcs11-1");
        return;
    }

    let _tpm = SwtpmInstance::new().expect("Failed to start TPM simulator");

    use avocado_cli::utils::pkcs11_devices::{get_pkcs11_module_path, DeviceType};
    use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};

    let module_path =
        get_pkcs11_module_path(&DeviceType::Tpm).expect("Failed to find PKCS#11 module path");

    println!("Using PKCS#11 module: {}", module_path.display());

    let pkcs11 = Pkcs11::new(module_path).expect("Failed to load PKCS#11 module");

    pkcs11
        .initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK))
        .expect("Failed to initialize PKCS#11");

    let slots = pkcs11
        .get_slots_with_token()
        .expect("Failed to get slots with tokens");

    if slots.is_empty() {
        eprintln!("No tokens found - this is expected for a fresh TPM");
        return;
    }

    println!("Found {} token(s)", slots.len());

    for slot in slots {
        let token_info = pkcs11
            .get_token_info(slot)
            .expect("Failed to get token info");
        println!("Token label: {}", token_info.label());
    }
}

#[test]
fn test_tpm_key_generation() {
    if !SwtpmInstance::is_available() {
        eprintln!("swtpm not available, skipping test. Install: sudo apt install swtpm");
        return;
    }

    if !tpm_pkcs11_available() {
        eprintln!("TPM PKCS#11 library not available, skipping test. Install: sudo apt install libtpm2-pkcs11-1");
        return;
    }

    let _tpm = SwtpmInstance::new().expect("Failed to start TPM simulator");

    use avocado_cli::utils::pkcs11_devices::{
        generate_keypair as generate_pkcs11_keypair, init_pkcs11_session, DeviceType, KeyAlgorithm,
        Pkcs11AuthMethod,
    };

    let auth_method = Pkcs11AuthMethod::None;
    let device_type = DeviceType::Tpm;

    let (_pkcs11, session) = init_pkcs11_session(&device_type, None, "", &auth_method)
        .expect("Failed to initialize PKCS#11 session");

    // Generate an ECC P-256 keypair
    let label = "test-tpm-key";
    let algorithm = KeyAlgorithm::EccP256;

    let (public_key_bytes, keyid, algo_str) = generate_pkcs11_keypair(&session, label, &algorithm)
        .expect("Failed to generate keypair in TPM");

    assert!(
        !public_key_bytes.is_empty(),
        "Public key bytes should not be empty"
    );
    assert!(
        keyid.starts_with("sha256-"),
        "KeyID should start with sha256-"
    );
    assert_eq!(algo_str, "ecdsa-p256", "Algorithm should be ecdsa-p256");

    println!("Generated TPM key:");
    println!("  KeyID: {keyid}");
    println!("  Algorithm: {algo_str}");
    println!("  Public key size: {} bytes", public_key_bytes.len());
}

#[test]
fn test_tpm_signing() {
    if !SwtpmInstance::is_available() {
        eprintln!("swtpm not available, skipping test. Install: sudo apt install swtpm");
        return;
    }

    if !tpm_pkcs11_available() {
        eprintln!("TPM PKCS#11 library not available, skipping test. Install: sudo apt install libtpm2-pkcs11-1");
        return;
    }

    let _tpm = SwtpmInstance::new().expect("Failed to start TPM simulator");

    use avocado_cli::utils::pkcs11_devices::{
        generate_keypair as generate_pkcs11_keypair, init_pkcs11_session, sign_with_pkcs11_device,
        DeviceType, KeyAlgorithm, Pkcs11AuthMethod,
    };
    use sha2::{Digest, Sha256};

    let auth_method = Pkcs11AuthMethod::None;
    let device_type = DeviceType::Tpm;

    let (_pkcs11, session) = init_pkcs11_session(&device_type, None, "", &auth_method)
        .expect("Failed to initialize PKCS#11 session");

    // Generate a keypair
    let label = "test-tpm-signing-key";
    let algorithm = KeyAlgorithm::EccP256;

    let (_public_key_bytes, _keyid, _algo_str) =
        generate_pkcs11_keypair(&session, label, &algorithm)
            .expect("Failed to generate keypair in TPM");

    // Create some test data to sign
    let test_data = b"Hello, TPM PKCS#11 signing!";
    let mut hasher = Sha256::new();
    hasher.update(test_data);
    let hash = hasher.finalize();

    // Sign the hash
    let signature =
        sign_with_pkcs11_device(&session, label, &hash, "").expect("Failed to sign data with TPM");

    assert!(!signature.is_empty(), "Signature should not be empty");
    println!("TPM signature size: {} bytes", signature.len());
}

/// Manual test instructions for hardware devices
#[test]
#[ignore]
fn test_manual_instructions() {
    println!("\n=== Manual Hardware Testing Instructions ===\n");
    println!("The automated tests use a software TPM (swtpm).");
    println!("To test with real hardware:\n");
    println!("1. For Physical TPM:");
    println!("   sudo apt install libtpm2-pkcs11-1 tpm2-tools");
    println!("   sudo usermod -a -G tss $USER  # Add user to TPM group");
    println!("   # Log out and back in");
    println!(
        "   avocado signing-keys create my-tpm-key --pkcs11-device tpm --generate --auth none\n"
    );
    println!("2. For YubiKey:");
    println!("   sudo apt install yubikey-manager libykcs11-1");
    println!("   # Insert YubiKey");
    println!("   avocado signing-keys create my-yk-key --pkcs11-device yubikey --generate --auth prompt\n");
    println!("3. Run integration tests:");
    println!("   cargo test --test pkcs11_integration_test\n");
}

#[test]
fn test_end_to_end_tpm_workflow() {
    if !SwtpmInstance::is_available() {
        eprintln!("swtpm not available, skipping test. Install: sudo apt install swtpm");
        return;
    }

    if !tpm_pkcs11_available() {
        eprintln!("TPM PKCS#11 library not available, skipping test. Install: sudo apt install libtpm2-pkcs11-1");
        return;
    }

    let _tpm = SwtpmInstance::new().expect("Failed to start TPM simulator");

    use avocado_cli::utils::pkcs11_devices::{
        build_pkcs11_uri, find_existing_key, generate_keypair as generate_pkcs11_keypair,
        init_pkcs11_session, sign_with_pkcs11_device, DeviceType, KeyAlgorithm, Pkcs11AuthMethod,
    };
    use sha2::{Digest, Sha256};

    let auth_method = Pkcs11AuthMethod::None;
    let device_type = DeviceType::Tpm;

    // Step 1: Initialize PKCS#11 session
    let (pkcs11, session) = init_pkcs11_session(&device_type, None, "", &auth_method)
        .expect("Failed to initialize PKCS#11 session");

    // Step 2: Generate a key
    let label = "e2e-test-key";
    let algorithm = KeyAlgorithm::EccP256;

    let (public_key_bytes, keyid, algo_str) =
        generate_pkcs11_keypair(&session, label, &algorithm).expect("Failed to generate keypair");

    println!("Step 1: Generated key with ID: {keyid}");
    assert_eq!(algo_str, "ecdsa-p256");

    // Step 3: Find the key we just created
    let (found_pubkey, found_keyid, _found_algo, _priv_label) =
        find_existing_key(&session, label).expect("Failed to find existing key");

    println!("Step 2: Found existing key: {found_keyid}");
    assert_eq!(keyid, found_keyid);
    assert_eq!(public_key_bytes.len(), found_pubkey.len());

    // Step 4: Build PKCS#11 URI
    let slot = session
        .get_session_info()
        .expect("Failed to get session info")
        .slot_id();
    let token_info = pkcs11
        .get_token_info(slot)
        .expect("Failed to get token info");
    let uri = build_pkcs11_uri(token_info.label(), label);

    println!("Step 3: Built URI: {uri}");
    assert!(uri.starts_with("pkcs11:"));

    // Step 5: Sign some data
    let test_data = b"End-to-end test data";
    let mut hasher = Sha256::new();
    hasher.update(test_data);
    let hash = hasher.finalize();

    let signature =
        sign_with_pkcs11_device(&session, label, &hash, "").expect("Failed to sign with TPM");

    println!(
        "Step 4: Signed data, signature size: {} bytes",
        signature.len()
    );
    assert!(!signature.is_empty());

    println!("\n✅ End-to-end TPM workflow test passed!");
}

#[test]
fn test_tpm_key_registration_and_removal() {
    if !SwtpmInstance::is_available() {
        eprintln!("swtpm not available, skipping test. Install: sudo apt install swtpm");
        return;
    }

    if !tpm_pkcs11_available() {
        eprintln!("TPM PKCS#11 library not available, skipping test. Install: sudo apt install libtpm2-pkcs11-1");
        return;
    }

    let _tpm = SwtpmInstance::new().expect("Failed to start TPM simulator");

    use avocado_cli::commands::signing_keys::create::SigningKeysCreateCommand;
    use avocado_cli::commands::signing_keys::remove::SigningKeysRemoveCommand;
    use avocado_cli::utils::signing_keys::KeysRegistry;
    use std::env;

    // Set up test environment
    let test_home = env::temp_dir().join(format!("avocado-test-{}", std::process::id()));
    fs::create_dir_all(&test_home).expect("Failed to create test home directory");
    env::set_var("HOME", &test_home);

    // Step 1: Create a TPM key using the command
    let key_name = "test-tpm-remove-key";
    let create_cmd = SigningKeysCreateCommand::new(
        Some(key_name.to_string()),
        None,
        Some("tpm".to_string()),
        None, // token - use first available
        Some("test-key-label".to_string()),
        false, // don't generate, reference existing
        "none".to_string(),
    );

    // First, generate the key in TPM directly
    use avocado_cli::utils::pkcs11_devices::{
        generate_keypair as generate_pkcs11_keypair, init_pkcs11_session, DeviceType, KeyAlgorithm,
        Pkcs11AuthMethod,
    };

    let auth_method = Pkcs11AuthMethod::None;
    let device_type = DeviceType::Tpm;

    let (_pkcs11, session) = init_pkcs11_session(&device_type, None, "", &auth_method)
        .expect("Failed to initialize PKCS#11 session");

    let (_public_key_bytes, keyid, _algo_str) =
        generate_pkcs11_keypair(&session, "test-key-label", &KeyAlgorithm::EccP256)
            .expect("Failed to generate keypair in TPM");

    println!("Generated TPM key with ID: {keyid}");

    // Step 2: Register the key using the create command
    create_cmd
        .execute()
        .expect("Failed to register TPM key with avocado");

    println!("Registered key '{key_name}' with avocado");

    // Step 3: Verify key is in registry
    let registry = KeysRegistry::load().expect("Failed to load registry");
    let entry = registry
        .get_key(key_name)
        .expect("Key should be in registry");

    assert_eq!(entry.keyid, keyid);
    println!("Verified key exists in registry");

    // Step 4: Remove the key by name
    let remove_cmd = SigningKeysRemoveCommand::new(key_name.to_string(), false);
    remove_cmd.execute().expect("Failed to remove key by name");

    println!("Removed key by name");

    // Step 5: Verify key is removed from registry
    let registry = KeysRegistry::load().expect("Failed to load registry after removal");
    assert!(
        registry.get_key(key_name).is_none(),
        "Key should be removed from registry"
    );

    println!("Verified key removed from registry");

    // Step 6: Test removal by key ID
    // Generate and register another key
    let key_name2 = "test-tpm-remove-key-2";
    let create_cmd2 = SigningKeysCreateCommand::new(
        Some(key_name2.to_string()),
        None,
        Some("tpm".to_string()),
        None,
        Some("test-key-label-2".to_string()),
        false,
        "none".to_string(),
    );

    let (_public_key_bytes2, keyid2, _algo_str2) =
        generate_pkcs11_keypair(&session, "test-key-label-2", &KeyAlgorithm::EccP256)
            .expect("Failed to generate second keypair in TPM");

    create_cmd2
        .execute()
        .expect("Failed to register second TPM key");

    println!("Generated and registered second key with ID: {keyid2}");

    // Remove by key ID
    let remove_cmd2 = SigningKeysRemoveCommand::new(keyid2.clone(), false);
    remove_cmd2
        .execute()
        .expect("Failed to remove key by key ID");

    println!("Removed key by key ID");

    // Verify removal
    let registry = KeysRegistry::load().expect("Failed to load registry after second removal");
    assert!(
        registry.get_key(key_name2).is_none(),
        "Second key should be removed from registry"
    );

    println!("Verified second key removed from registry");

    // Clean up test home
    let _ = fs::remove_dir_all(&test_home);

    println!("\n✅ TPM key registration and removal test passed!");
}
