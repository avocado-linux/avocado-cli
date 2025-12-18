//! PKCS#11 device integration for hardware-backed signing keys.
//!
//! Provides unified support for TPM, YubiKey, HSMs, and other PKCS#11-compatible devices.

use anyhow::{Context, Result};
use cryptoki::context::{CInitializeArgs, Pkcs11};
use cryptoki::mechanism::Mechanism;
use cryptoki::object::{Attribute, AttributeType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::slot::Slot;
use cryptoki::types::AuthPin;
use sha2::{Digest, Sha256};
use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

/// Supported hardware device types
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceType {
    Tpm,
    Yubikey,
    Auto,
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DeviceType::Tpm => write!(f, "TPM"),
            DeviceType::Yubikey => write!(f, "YubiKey"),
            DeviceType::Auto => write!(f, "Auto"),
        }
    }
}

impl FromStr for DeviceType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "tpm" => Ok(DeviceType::Tpm),
            "yubikey" | "yk" => Ok(DeviceType::Yubikey),
            "auto" => Ok(DeviceType::Auto),
            _ => anyhow::bail!(
                "Unsupported device type '{}'. Supported: tpm, yubikey, auto",
                s
            ),
        }
    }
}

/// Authentication methods for PKCS#11 devices
#[derive(Debug, Clone)]
pub enum Pkcs11AuthMethod {
    None,
    Prompt,
    EnvVar(String),
}

impl FromStr for Pkcs11AuthMethod {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Pkcs11AuthMethod::None),
            "prompt" => Ok(Pkcs11AuthMethod::Prompt),
            "env" => Ok(Pkcs11AuthMethod::EnvVar("AVOCADO_PKCS11_PIN".to_string())),
            _ => anyhow::bail!(
                "Unsupported auth method '{}'. Supported: none, prompt, env",
                s
            ),
        }
    }
}

/// Supported key algorithms
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAlgorithm {
    EccP256,
    Rsa2048,
}

impl KeyAlgorithm {
    pub fn as_str(&self) -> &str {
        match self {
            KeyAlgorithm::EccP256 => "ecdsa-p256",
            KeyAlgorithm::Rsa2048 => "rsa2048",
        }
    }
}

/// Get PKCS#11 module path for a device type
pub fn get_pkcs11_module_path(device_type: &DeviceType) -> Result<PathBuf> {
    // 1. Check PKCS11_MODULE_PATH env var (highest priority)
    if let Ok(path) = env::var("PKCS11_MODULE_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!(
            "PKCS11_MODULE_PATH set but file does not exist: {}",
            p.display()
        );
    }

    // 2. Search for modules dynamically (architecture-agnostic)
    let module_names = match device_type {
        DeviceType::Tpm => vec!["libtpm2_pkcs11.so"],
        DeviceType::Yubikey => vec!["libykcs11.so", "opensc-pkcs11.so"],
        DeviceType::Auto => vec!["libtpm2_pkcs11.so", "libykcs11.so", "opensc-pkcs11.so"],
    };

    // Search in standard library directories
    let search_dirs = get_library_search_paths();

    for dir in &search_dirs {
        for module_name in &module_names {
            // Check with exact name
            let path = dir.join(module_name);
            if path.exists() {
                return Ok(path);
            }

            // Check in pkcs11 subdirectory
            let pkcs11_path = dir.join("pkcs11").join(module_name);
            if pkcs11_path.exists() {
                return Ok(pkcs11_path);
            }

            // Check for versioned .so files (e.g., .so.1, .so.1.9.0)
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(module_name) {
                        return Ok(entry.path());
                    }
                }
            }

            // Check pkcs11 subdirectory for versioned files
            let pkcs11_dir = dir.join("pkcs11");
            if let Ok(entries) = fs::read_dir(&pkcs11_dir) {
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(module_name) {
                        return Ok(entry.path());
                    }
                }
            }
        }
    }

    anyhow::bail!(
        "{} PKCS#11 module not found. Set PKCS11_MODULE_PATH or install the appropriate package:\n  \
        - TPM: libtpm2-pkcs11-1 (Ubuntu/Debian) or tpm2-pkcs11 (Fedora/Arch)\n  \
        - YubiKey: libykcs11-1 or opensc-pkcs11 (Ubuntu/Debian) or ykcs11/opensc (Fedora/Arch)\n\
        \n\
        Searched in: {}",
        device_type,
        search_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )
}

/// Get standard library search paths for the current system
fn get_library_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // 1. Check p11-kit module directory (standard on most systems)
    if let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--variable=p11_module_path", "p11-kit-1"])
        .output()
    {
        if output.status.success() {
            let path_str = String::from_utf8_lossy(&output.stdout);
            let path = PathBuf::from(path_str.trim());
            if path.exists() {
                paths.push(path);
            }
        }
    }

    // 2. Multi-arch library directories (works for all architectures)
    if let Ok(output) = std::process::Command::new("gcc")
        .args(["-print-multiarch"])
        .output()
    {
        if output.status.success() {
            let multiarch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !multiarch.is_empty() {
                paths.push(PathBuf::from(format!("/usr/lib/{}", multiarch)));
            }
        }
    }

    // 3. Standard library paths (work on all architectures)
    paths.extend(vec![
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/usr/lib64"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
    ]);

    // 4. Check LD_LIBRARY_PATH
    if let Ok(ld_path) = env::var("LD_LIBRARY_PATH") {
        for path in ld_path.split(':') {
            if !path.is_empty() {
                paths.push(PathBuf::from(path));
            }
        }
    }

    // 5. Architecture-specific paths as fallback (detected from system)
    #[cfg(target_arch = "x86_64")]
    paths.extend(vec![PathBuf::from("/usr/lib/x86_64-linux-gnu")]);

    #[cfg(target_arch = "aarch64")]
    paths.extend(vec![PathBuf::from("/usr/lib/aarch64-linux-gnu")]);

    #[cfg(target_arch = "arm")]
    paths.extend(vec![
        PathBuf::from("/usr/lib/arm-linux-gnueabihf"),
        PathBuf::from("/usr/lib/arm-linux-gnueabi"),
    ]);

    #[cfg(target_arch = "riscv64")]
    paths.extend(vec![PathBuf::from("/usr/lib/riscv64-linux-gnu")]);

    paths
}

/// Get authentication PIN/password from user
pub fn get_device_auth(method: &Pkcs11AuthMethod) -> Result<String> {
    match method {
        Pkcs11AuthMethod::None => Ok(String::new()),
        Pkcs11AuthMethod::Prompt => {
            let pin = rpassword::prompt_password("Enter PIN for PKCS#11 device: ")
                .context("Failed to read PIN from prompt")?;
            Ok(pin)
        }
        Pkcs11AuthMethod::EnvVar(var_name) => env::var(var_name).with_context(|| {
            format!(
                "Environment variable '{}' not set. Set it or use --auth prompt",
                var_name
            )
        }),
    }
}

/// Discover a device token by device type and optional token label
pub fn discover_device_token(
    pkcs11: &Pkcs11,
    device_type: &DeviceType,
    token_label: Option<&str>,
) -> Result<(Slot, cryptoki::slot::TokenInfo)> {
    let slots = pkcs11
        .get_slots_with_token()
        .context("Failed to get PKCS#11 slots with tokens")?;

    if slots.is_empty() {
        let help_msg = match device_type {
            DeviceType::Tpm => {
                "No TPM tokens found. Initialize the TPM PKCS#11 token first:\n\
                \n\
                1. Ensure you're in the 'tss' group:\n\
                   sudo usermod -a -G tss $USER\n\
                   newgrp tss\n\
                \n\
                2. Initialize the TPM PKCS#11 store:\n\
                   mkdir -p ~/.tpm2_pkcs11\n\
                   tpm2_ptool init\n\
                \n\
                3. Create a token:\n\
                   tpm2_ptool addtoken --pid=1 --label=avocado --userpin=yourpin --sopin=yoursopin\n\
                \n\
                4. Verify:\n\
                   tpm2_ptool listtoken"
            }
            DeviceType::Yubikey => {
                "No YubiKey tokens found. Ensure:\n\
                1. YubiKey is inserted\n\
                2. PIV application is initialized (use 'ykman piv info')"
            }
            DeviceType::Auto => {
                "No PKCS#11 tokens found. Ensure the device is connected and initialized."
            }
        };
        anyhow::bail!("{}", help_msg);
    }

    // If a specific token label was provided, look for exact match
    if let Some(requested_label) = token_label {
        for slot in &slots {
            let token_info = pkcs11
                .get_token_info(*slot)
                .context("Failed to get token info")?;

            if token_info.label().trim() == requested_label.trim() {
                return Ok((*slot, token_info));
            }
        }

        // Token not found - collect available tokens for error message
        let mut available_tokens = Vec::new();
        for slot in &slots {
            if let Ok(token_info) = pkcs11.get_token_info(*slot) {
                available_tokens.push(token_info.label().to_string());
            }
        }

        anyhow::bail!(
            "Token '{}' not found. Available tokens: {}",
            requested_label,
            available_tokens.join(", ")
        );
    }

    // No specific token requested - use first available token
    if !slots.is_empty() {
        let slot = slots[0];
        let token_info = pkcs11
            .get_token_info(slot)
            .context("Failed to get token info")?;
        return Ok((slot, token_info));
    }

    // If no match found but we have tokens, return error with available tokens
    let mut available_tokens = Vec::new();
    for slot in pkcs11.get_slots_with_token()? {
        if let Ok(token_info) = pkcs11.get_token_info(slot) {
            available_tokens.push(token_info.label().to_string());
        }
    }

    anyhow::bail!(
        "No matching {} token found. Available tokens: {}",
        device_type,
        available_tokens.join(", ")
    )
}

/// Generate a keypair in the PKCS#11 device
pub fn generate_keypair(
    session: &Session,
    label: &str,
    algorithm: &KeyAlgorithm,
) -> Result<(Vec<u8>, String, String)> {
    let mechanism = match algorithm {
        KeyAlgorithm::EccP256 => Mechanism::EccKeyPairGen,
        KeyAlgorithm::Rsa2048 => Mechanism::RsaPkcsKeyPairGen,
    };

    // Build public key template
    let mut pub_key_template = vec![
        Attribute::Token(true),
        Attribute::Label(label.as_bytes().to_vec()),
        Attribute::Verify(true),
    ];

    // Build private key template
    let priv_key_template = vec![
        Attribute::Token(true),
        Attribute::Label(label.as_bytes().to_vec()),
        Attribute::Sign(true),
        Attribute::Sensitive(true),
        Attribute::Private(true),
    ];

    // Add algorithm-specific attributes
    match algorithm {
        KeyAlgorithm::EccP256 => {
            // NIST P-256 curve OID: 1.2.840.10045.3.1.7
            let p256_oid = vec![0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
            pub_key_template.push(Attribute::EcParams(p256_oid));
        }
        KeyAlgorithm::Rsa2048 => {
            pub_key_template.push(Attribute::ModulusBits(2048.into()));
            pub_key_template.push(Attribute::PublicExponent(vec![0x01, 0x00, 0x01]));
            // 65537
        }
    }

    let (public_key, _private_key) = session
        .generate_key_pair(&mechanism, &pub_key_template, &priv_key_template)
        .context("Failed to generate key pair in PKCS#11 device")?;

    // Extract public key bytes for keyid generation
    let public_key_bytes = extract_public_key_bytes(session, public_key, algorithm)?;

    // Generate keyid from public key
    let keyid = generate_keyid_from_public_key(&public_key_bytes);

    Ok((public_key_bytes, keyid, algorithm.as_str().to_string()))
}

/// Find an existing key by label
/// Find an existing PKCS#11 keypair and return (public_key_bytes, keyid, algorithm, private_key_label)
pub fn find_existing_key(
    session: &Session,
    label: &str,
) -> Result<(Vec<u8>, String, String, String)> {
    // Try exact match first
    let template = vec![
        Attribute::Label(label.as_bytes().to_vec()),
        Attribute::Class(ObjectClass::PUBLIC_KEY),
    ];

    let mut objects = session
        .find_objects(&template)
        .context("Failed to find key objects")?;

    // If no exact match, try to find any public key and match by trimmed label
    if objects.is_empty() {
        let all_keys_template = vec![Attribute::Class(ObjectClass::PUBLIC_KEY)];

        let all_objects = session
            .find_objects(&all_keys_template)
            .context("Failed to list all public keys")?;

        // Check each object's label
        for obj in all_objects {
            let attrs = session
                .get_attributes(obj, &[AttributeType::Label])
                .context("Failed to get key label")?;

            if let Some(Attribute::Label(obj_label)) = attrs.first() {
                let obj_label_str = String::from_utf8_lossy(obj_label).trim().to_string();
                if obj_label_str == label.trim() {
                    objects = vec![obj];
                    break;
                }
            }
        }
    }

    if objects.is_empty() {
        // List available keys for helpful error message
        let all_keys_template = vec![Attribute::Class(ObjectClass::PUBLIC_KEY)];
        let all_objects = session.find_objects(&all_keys_template).unwrap_or_default();

        let mut available_labels = Vec::new();
        for obj in all_objects {
            if let Ok(attrs) = session.get_attributes(obj, &[AttributeType::Label]) {
                if let Some(Attribute::Label(obj_label)) = attrs.first() {
                    available_labels
                        .push(format!("'{}'", String::from_utf8_lossy(obj_label).trim()));
                }
            }
        }

        anyhow::bail!(
            "No key found with label '{}'. Available keys: {}",
            label,
            if available_labels.is_empty() {
                "none".to_string()
            } else {
                available_labels.join(", ")
            }
        );
    }

    let public_key_handle = objects[0];

    // Get the public key's CKA_ID to find the matching private key
    let id_attrs = session
        .get_attributes(public_key_handle, &[AttributeType::Id])
        .context("Failed to get key ID")?;

    // Find the corresponding private key using the same CKA_ID
    let private_key_label = if let Some(Attribute::Id(key_id)) = id_attrs.first() {
        let priv_template = vec![
            Attribute::Id(key_id.clone()),
            Attribute::Class(ObjectClass::PRIVATE_KEY),
        ];

        let priv_objects = session
            .find_objects(&priv_template)
            .context("Failed to find private key")?;

        if !priv_objects.is_empty() {
            let priv_label_attrs = session
                .get_attributes(priv_objects[0], &[AttributeType::Label])
                .context("Failed to get private key label")?;

            if let Some(Attribute::Label(priv_label)) = priv_label_attrs.first() {
                String::from_utf8_lossy(priv_label).trim().to_string()
            } else {
                label.to_string() // Fallback to input label
            }
        } else {
            label.to_string() // Fallback to input label
        }
    } else {
        label.to_string() // Fallback to input label
    };

    // Determine algorithm from key type
    let key_type_attr = session
        .get_attributes(public_key_handle, &[AttributeType::KeyType])
        .context("Failed to get key type")?;

    let algorithm = detect_algorithm_from_attributes(&key_type_attr)?;

    // Extract public key bytes
    let public_key_bytes = extract_public_key_bytes(session, public_key_handle, &algorithm)?;

    // Generate keyid
    let keyid = generate_keyid_from_public_key(&public_key_bytes);

    Ok((
        public_key_bytes,
        keyid,
        algorithm.as_str().to_string(),
        private_key_label,
    ))
}

/// Extract public key bytes from a PKCS#11 object
fn extract_public_key_bytes(
    session: &Session,
    public_key_handle: ObjectHandle,
    algorithm: &KeyAlgorithm,
) -> Result<Vec<u8>> {
    match algorithm {
        KeyAlgorithm::EccP256 => {
            // For EC keys, get the EC_POINT attribute
            let attrs = session
                .get_attributes(public_key_handle, &[AttributeType::EcPoint])
                .context("Failed to get EC_POINT attribute")?;

            for attr in attrs {
                if let Attribute::EcPoint(point) = attr {
                    return Ok(point);
                }
            }

            anyhow::bail!("EC_POINT attribute not found")
        }
        KeyAlgorithm::Rsa2048 => {
            // For RSA keys, get the modulus
            let attrs = session
                .get_attributes(public_key_handle, &[AttributeType::Modulus])
                .context("Failed to get modulus attribute")?;

            for attr in attrs {
                if let Attribute::Modulus(modulus) = attr {
                    return Ok(modulus);
                }
            }

            anyhow::bail!("Modulus attribute not found")
        }
    }
}

/// Detect algorithm from PKCS#11 attributes
fn detect_algorithm_from_attributes(attributes: &[Attribute]) -> Result<KeyAlgorithm> {
    for attr in attributes {
        if let Attribute::KeyType(key_type) = attr {
            match *key_type {
                cryptoki::object::KeyType::EC => {
                    // Default to P-256 for EC keys
                    // Could inspect EC_PARAMS to determine exact curve
                    return Ok(KeyAlgorithm::EccP256);
                }
                cryptoki::object::KeyType::RSA => {
                    // Default to RSA-2048
                    // Could inspect modulus length to determine exact size
                    return Ok(KeyAlgorithm::Rsa2048);
                }
                _ => continue,
            }
        }
    }

    anyhow::bail!("Unable to determine key algorithm from attributes")
}

/// Generate a keyid from public key bytes (SHA-256, first 16 hex chars)
pub fn generate_keyid_from_public_key(public_key_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key_bytes);
    let hash = hasher.finalize();
    format!("sha256-{}", hex_encode(&hash[..8]))
}

/// Build a PKCS#11 URI
pub fn build_pkcs11_uri(token_label: &str, object_label: &str) -> String {
    format!(
        "pkcs11:token={};object={};type=private",
        uri_encode(token_label),
        uri_encode(object_label)
    )
}

/// Parse a PKCS#11 URI to extract token and object labels
pub fn parse_pkcs11_uri(uri: &str) -> Result<(String, String)> {
    if !uri.starts_with("pkcs11:") {
        anyhow::bail!("Invalid PKCS#11 URI: must start with 'pkcs11:'");
    }

    let params = &uri[7..]; // Skip "pkcs11:"
    let mut token_label = None;
    let mut object_label = None;

    for param in params.split(';') {
        if let Some((key, value)) = param.split_once('=') {
            match key {
                "token" => token_label = Some(uri_decode(value)?),
                "object" => object_label = Some(uri_decode(value)?),
                _ => {} // Ignore other parameters
            }
        }
    }

    let token =
        token_label.ok_or_else(|| anyhow::anyhow!("PKCS#11 URI missing 'token' parameter"))?;
    let object =
        object_label.ok_or_else(|| anyhow::anyhow!("PKCS#11 URI missing 'object' parameter"))?;

    Ok((token, object))
}

/// URI-encode a string (simple implementation for labels)
fn uri_encode(s: &str) -> String {
    s.replace(' ', "%20")
        .replace(';', "%3B")
        .replace('=', "%3D")
}

/// URI-decode a string (simple implementation for labels)
fn uri_decode(s: &str) -> Result<String> {
    let decoded = s
        .replace("%20", " ")
        .replace("%3B", ";")
        .replace("%3D", "=");
    Ok(decoded)
}

/// Hex encode bytes
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Initialize PKCS#11 and open session
pub fn init_pkcs11_session(
    device_type: &DeviceType,
    token_label: Option<&str>,
    auth: &str,
    _auth_method: &Pkcs11AuthMethod,
) -> Result<(Pkcs11, Session)> {
    // Get module path
    let module_path = get_pkcs11_module_path(device_type)?;

    // Initialize PKCS#11
    let pkcs11 = Pkcs11::new(module_path).context("Failed to load PKCS#11 module")?;

    pkcs11
        .initialize(CInitializeArgs::OsThreads)
        .context("Failed to initialize PKCS#11")?;

    // Find token
    let (slot, _token_info) = discover_device_token(&pkcs11, device_type, token_label)?;

    // Open session
    let session = pkcs11
        .open_rw_session(slot)
        .context("Failed to open PKCS#11 session")?;

    // Login - auth should contain the PIN already
    if !auth.is_empty() {
        let auth_pin = AuthPin::new(auth.to_string());
        session
            .login(UserType::User, Some(&auth_pin))
            .context("Failed to login to PKCS#11 device")?;
    }

    Ok((pkcs11, session))
}

/// Delete a PKCS#11 key from hardware device
pub fn delete_pkcs11_key(uri: &str) -> Result<()> {
    // Parse the URI to get token and object label
    let (token_label, object_label) = parse_pkcs11_uri(uri)?;

    // Determine device type from token label (best effort)
    let device_type = if token_label.to_lowercase().contains("tpm") {
        DeviceType::Tpm
    } else if token_label.to_lowercase().contains("yubi")
        || token_label.to_lowercase().contains("piv")
    {
        DeviceType::Yubikey
    } else {
        DeviceType::Auto
    };

    // Get module path
    let module_path = get_pkcs11_module_path(&device_type)?;
    let pkcs11 = Pkcs11::new(module_path).context("Failed to load PKCS#11 module")?;

    pkcs11
        .initialize(CInitializeArgs::OsThreads)
        .context("Failed to initialize PKCS#11")?;

    // Find the token
    let (slot, _token_info) = discover_device_token(&pkcs11, &device_type, Some(&token_label))?;

    // Open a session
    let session = pkcs11
        .open_rw_session(slot)
        .context("Failed to open session")?;

    // For deletion, we need to login with PIN
    let pin_str = rpassword::prompt_password("Enter PIN to delete hardware key: ")
        .context("Failed to read PIN")?;
    let auth_pin = AuthPin::new(pin_str.clone());

    session
        .login(UserType::User, Some(&auth_pin))
        .context("Failed to login to PKCS#11 device")?;

    // Find the private key object
    let template = vec![
        Attribute::Label(object_label.as_bytes().to_vec()),
        Attribute::Class(ObjectClass::PRIVATE_KEY),
    ];

    let objects = session
        .find_objects(&template)
        .context("Failed to find objects")?;

    if objects.is_empty() {
        anyhow::bail!(
            "Private key '{}' not found in hardware device",
            object_label
        );
    }

    let private_key_handle = objects[0];

    // Delete the private key
    session
        .destroy_object(private_key_handle)
        .context("Failed to delete private key from device")?;

    // Also try to delete the corresponding public key
    let pub_template = vec![
        Attribute::Label(object_label.as_bytes().to_vec()),
        Attribute::Class(ObjectClass::PUBLIC_KEY),
    ];

    let pub_objects = session
        .find_objects(&pub_template)
        .context("Failed to find public key")?;

    if !pub_objects.is_empty() {
        let public_key_handle = pub_objects[0];
        let _ = session.destroy_object(public_key_handle); // Best effort, ignore errors
    }

    Ok(())
}

/// Sign data using a PKCS#11 device
pub fn sign_with_pkcs11_device(
    session: &Session,
    object_label: &str,
    data: &[u8],
    pin: &str,
) -> Result<Vec<u8>> {
    // Find private key
    let template = vec![
        Attribute::Label(object_label.as_bytes().to_vec()),
        Attribute::Class(ObjectClass::PRIVATE_KEY),
    ];

    let objects = session
        .find_objects(&template)
        .context("Failed to find private key object")?;

    if objects.is_empty() {
        anyhow::bail!("No private key found with label '{}'", object_label);
    }

    let private_key_handle = objects[0];

    // Check if the key requires always-authenticate
    let always_auth_attr = session
        .get_attributes(private_key_handle, &[AttributeType::AlwaysAuthenticate])
        .ok();

    let requires_auth = if let Some(attrs) = always_auth_attr {
        if let Some(Attribute::AlwaysAuthenticate(val)) = attrs.first() {
            *val
        } else {
            false
        }
    } else {
        false
    };

    if requires_auth {
        // Key requires per-operation authentication (common with YubiKey)
        // Use the provided PIN for context-specific login
        let auth_pin = AuthPin::new(pin.to_string());

        // Context-specific login for this operation
        session
            .login(UserType::ContextSpecific, Some(&auth_pin))
            .context("Failed to authenticate for signing operation")?;
    }

    // Determine the mechanism based on key type
    let key_type_attr = session
        .get_attributes(private_key_handle, &[AttributeType::KeyType])
        .context("Failed to get key type")?;

    let mechanism = if let Some(Attribute::KeyType(key_type)) = key_type_attr.first() {
        match *key_type {
            cryptoki::object::KeyType::EC => Mechanism::Ecdsa,
            cryptoki::object::KeyType::RSA => Mechanism::RsaPkcs,
            _ => anyhow::bail!("Unsupported key type for signing"),
        }
    } else {
        anyhow::bail!("Unable to determine key type");
    };

    // Sign the data
    let signature = session
        .sign(&mechanism, private_key_handle, data)
        .context("Failed to sign data with PKCS#11 device")?;

    Ok(signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_type_from_str() {
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
    fn test_auth_method_from_str() {
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
    }

    #[test]
    fn test_build_pkcs11_uri() {
        let uri = build_pkcs11_uri("MyToken", "signing-key");
        assert_eq!(uri, "pkcs11:token=MyToken;object=signing-key;type=private");

        let uri = build_pkcs11_uri("Token With Spaces", "key label");
        assert!(uri.contains("%20"));
    }

    #[test]
    fn test_parse_pkcs11_uri() {
        let (token, object) = parse_pkcs11_uri("pkcs11:token=MyToken;object=signing-key").unwrap();
        assert_eq!(token, "MyToken");
        assert_eq!(object, "signing-key");

        let (token, object) =
            parse_pkcs11_uri("pkcs11:token=Token%20Name;object=key%20label;type=private").unwrap();
        assert_eq!(token, "Token Name");
        assert_eq!(object, "key label");

        assert!(parse_pkcs11_uri("invalid").is_err());
        assert!(parse_pkcs11_uri("pkcs11:token=Only").is_err());
    }

    #[test]
    fn test_generate_keyid_from_public_key() {
        let test_key = b"test public key data";
        let keyid = generate_keyid_from_public_key(test_key);
        assert!(keyid.starts_with("sha256-"));
        assert_eq!(keyid.len(), 7 + 16); // "sha256-" + 16 hex chars
    }
}
