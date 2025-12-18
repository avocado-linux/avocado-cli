# Signing Keys Configuration

## Overview

The avocado CLI supports managing signing keys for runtime image signing through two mechanisms:

1. **Global Registry**: Keys stored in platform-specific config directories via `avocado signing-keys` commands
2. **Local Config Bridge**: Map friendly names to key IDs in your `avocado.yaml` config

## Global Key Management

### Creating Keys

```bash
# Create a new ed25519 key with a name
avocado signing-keys create my-production-key

# Create a key (defaults to key ID as name)
avocado signing-keys create

# Register a hardware-backed PKCS#11 key (manual method)
avocado signing-keys create yubikey-signing --uri "pkcs11:token=YubiKey;object=signing-key"
```

### Creating Hardware-Backed Keys (TPM, YubiKey, HSMs)

The avocado CLI supports hardware-backed signing keys via PKCS#11, providing unified support for TPM, YubiKey, HSMs, and other PKCS#11-compatible devices.

#### TPM 2.0 Keys

**Prerequisites:**
```bash
# Install TPM PKCS#11 module (Ubuntu/Debian)
sudo apt install libtpm2-pkcs11-1 libtpm2-pkcs11-tools tpm2-tools

# Add your user to the tss group for TPM access
sudo usermod -a -G tss $USER

# Log out and back in (or use newgrp)
newgrp tss

# For other distros:
# Fedora/RHEL: sudo dnf install tpm2-pkcs11 tpm2-tools
# Arch: sudo pacman -S tpm2-pkcs11 tpm2-tools

# Verify installation (the CLI will auto-detect the library location)
# On x86_64: /usr/lib/x86_64-linux-gnu/pkcs11/libtpm2_pkcs11.so
# On ARM64: /usr/lib/aarch64-linux-gnu/pkcs11/libtpm2_pkcs11.so
# The CLI searches all architecture-specific paths automatically
```

**Initialize TPM PKCS#11 Token (First Time Setup):**
```bash
# Create a PKCS#11 store in the TPM
mkdir -p ~/.tpm2_pkcs11
tpm2_ptool init

# Create a new token (you'll be prompted to set a PIN)
tpm2_ptool addtoken --pid=1 --label=avocado --userpin=yourpin --sopin=yoursopin

# Verify the token is created
tpm2_ptool listtoken
```

**Generate a new key in TPM:**
```bash
# Generate with PIN prompt (specify token name)
avocado signing-keys create my-tpm-key --pkcs11-device tpm --token avocado --generate --auth prompt

# Generate with PIN from environment variable
export AVOCADO_PKCS11_PIN=your-tpm-pin
avocado signing-keys create my-tpm-key --pkcs11-device tpm --token avocado --generate --auth env

# Generate without specifying token (uses first available)
avocado signing-keys create my-tpm-key --pkcs11-device tpm --generate --auth prompt

# Generate without PIN (if TPM has no auth)
avocado signing-keys create my-tpm-key --pkcs11-device tpm --token avocado --generate --auth none
```

**Reference an existing TPM key:**
```bash
# Reference key by label
avocado signing-keys create prod-key --pkcs11-device tpm --token avocado --key-label existing-tpm-key --auth prompt
```

#### YubiKey Keys

**Prerequisites:**
```bash
# Ubuntu/Debian - Option 1: YubiKey manager (recommended)
sudo apt install yubikey-manager libykcs11-1

# Ubuntu/Debian - Option 2: OpenSC (alternative)
sudo apt install opensc-pkcs11

# For other distros:
# Fedora/RHEL: sudo dnf install ykcs11 (or opensc)
# Arch: sudo pacman -S yubikey-manager (or opensc)

# Verify installation (the CLI will auto-detect the library location)
# The CLI automatically finds libraries across all architectures

# Optional: Set module path explicitly only if auto-detection fails
export PKCS11_MODULE_PATH=/path/to/your/libykcs11.so
```

**Generate a new key in YubiKey:**
```bash
# Generate in YubiKey PIV slot
avocado signing-keys create yk-prod --pkcs11-device yubikey --generate --auth prompt

# YubiKey will prompt for PIN (default: 123456)
# Consider requiring touch for signing operations (configure via ykman)
```

**Reference an existing YubiKey key:**
```bash
# Reference existing PIV key
avocado signing-keys create yk-key --pkcs11-device yubikey --key-label "PIV AUTH key" --auth prompt
```

#### Manual PKCS#11 URI Registration (Advanced)

For other devices or custom configurations, you can manually register PKCS#11 keys using URIs:

```bash
# Register any PKCS#11 device
avocado signing-keys create custom-hsm --uri "pkcs11:token=MyHSM;object=signing-key"

# Example with more URI parameters
avocado signing-keys create hsm-prod --uri "pkcs11:token=Luna%20SA;object=prod-signing;type=private"
```

#### Hardware Key Algorithm Support

Hardware devices support different algorithms than file-based keys:

| Device Type | Supported Algorithms | Default |
|-------------|---------------------|---------|
| File-based | Ed25519 | Ed25519 |
| TPM 2.0 | ECC P-256, RSA-2048 | ECC P-256 |
| YubiKey | ECC P-256, RSA-2048 | ECC P-256 |
| HSMs | Varies by device | Device-dependent |

**Note:** Most hardware devices do NOT support Ed25519. The CLI automatically uses ECC P-256 for hardware keys as it's universally supported.

#### Authentication Methods

Three authentication methods are supported:

1. **`--auth prompt`** (default): Interactively prompts for PIN/password
   ```bash
   avocado signing-keys create my-key --pkcs11-device tpm --generate --auth prompt
   ```

2. **`--auth env`**: Reads PIN from `AVOCADO_PKCS11_PIN` environment variable
   ```bash
   export AVOCADO_PKCS11_PIN=my-secure-pin
   avocado signing-keys create my-key --pkcs11-device tpm --generate --auth env
   ```

3. **`--auth none`**: No authentication (for devices without PIN protection)
   ```bash
   avocado signing-keys create my-key --pkcs11-device tpm --generate --auth none
   ```

### Listing Keys

```bash
avocado signing-keys list
```

Output:
```
Registered signing keys:

  my-production-key
    Key ID:    sha256-7ca821b2d4ac87b3
    Algorithm: ed25519
    Type:      file
    Created:   2025-12-17 15:10:22 UTC
```

### Removing Keys

```bash
avocado signing-keys remove my-production-key
```

## Configuration Format

### Mapping Keys in avocado.yaml

The `signing_keys` section creates a local mapping between friendly names and key IDs:

```yaml
signing_keys:
  - production-key: sha256-abc123def456
  - staging-key: sha256-789012fedcba
  - backup-key: sha256-111222333444
```

### Referencing Keys in Runtimes

Each runtime can reference a signing key by name with optional checksum algorithm:

```yaml
runtime:
  production:
    dependencies:
      avocado-img-bootfiles: "*"
      avocado-img-rootfs: "*"
    signing:
      key: production-key
      checksum_algorithm: blake3  # Optional, defaults to sha256

  staging:
    signing:
      key: staging-key
      # checksum_algorithm defaults to sha256 if not specified
  
  dev:
    # No signing configuration - unsigned builds
    dependencies:
      avocado-img-bootfiles: "*"
```

### Supported Checksum Algorithms

- **sha256** (default): SHA-256 checksums
- **blake3**: BLAKE3 checksums (faster than SHA-256)

## Complete Example

```yaml
default_target: qemux86-64

sdk:
  image: ghcr.io/avocado-framework/avocado-sdk:latest

# Map friendly names to key IDs from global registry
signing_keys:
  - production-key: sha256-abc123def456
  - staging-key: sha256-789012fedcba

runtime:
  production:
    signing:
      key: production-key
      checksum_algorithm: blake3
  
  staging:
    signing:
      key: staging-key
```

## Key Storage Locations

Keys are stored in platform-specific directories:

- **Linux**: `~/.config/avocado/signing-keys/`
- **macOS**: `~/Library/Application Support/avocado/signing-keys/`
- **Windows**: `C:\Users\<User>\AppData\Roaming\avocado\signing-keys\`

## Key Registry Format

The global registry is stored in `keys.json`:

```json
{
  "keys": {
    "my-production-key": {
      "keyid": "sha256-abc123def456",
      "algorithm": "ed25519",
      "created_at": "2025-12-17T10:30:00Z",
      "uri": "file:///home/user/.config/avocado/signing-keys/sha256-abc123"
    }
  }
}
```

## API Usage

For programmatic access, the following methods are available:

```rust
use avocado_cli::utils::config::Config;

let config = Config::load("avocado.yaml")?;

// Get all signing keys
let keys = config.get_signing_keys();

// Get specific key ID by name
let keyid = config.get_signing_key_id("production-key");

// Get signing key for a runtime
let runtime_key = config.get_runtime_signing_key("production");
```

## Image Signing

When you build a runtime with signing configured, the build process uses a **multi-pass architecture** to sign images securely:

### Multi-Pass Signing Workflow

The signing process is split into three distinct passes to support both file-based keys and hardware-backed keys (TPM/YubiKey via PKCS#11):

```
┌─────────────────────────────────────────────────────────────┐
│ Pass 1: Checksum Generation (Inside Container)              │
│ ───────────────────────────────────────────────            │
│ 1. Container accesses images in Docker volume              │
│ 2. Computes checksums ONLY for required extension .raw files│
│    using sha256sum or b3sum                                │
│ 3. Saves checksums as .sha256 or .blake3 files next to     │
│    each image in runtimes/<runtime_name>/extensions/       │
└─────────────────────────────────────────────────────────────┘
                           ↓ (checksum files)
┌─────────────────────────────────────────────────────────────┐
│ Pass 2: Checksum Extraction (Docker cp)                     │
│ ────────────────────────────────────                       │
│ 1. Extracts .sha256/.blake3 files from volume              │
│ 2. Parses checksums into manifest                          │
└─────────────────────────────────────────────────────────────┘
                           ↓ (manifest)
┌─────────────────────────────────────────────────────────────┐
│ Pass 3: Signing (On Host)                                   │
│ ──────────────────────                                     │
│ 1. Host receives checksum manifest                         │
│ 2. Signs each checksum using signing key:                  │
│    • File-based keys: Load from ~/.config/avocado/         │
│    • PKCS#11 keys: Access TPM/YubiKey directly            │
│ 3. Generates .sig files                                    │
└─────────────────────────────────────────────────────────────┘
                           ↓ (signatures)
┌─────────────────────────────────────────────────────────────┐
│ Pass 4: Signature Writing (Docker cp)                       │
│ ────────────────────────────────────                       │
│ 1. Creates temporary container with volume                 │
│ 2. Copies .sig files into volume via docker cp             │
│ 3. Removes temporary container                             │
└─────────────────────────────────────────────────────────────┘
```

### Why Multi-Pass?

This architecture solves a key challenge: **Docker volumes are not directly accessible from the host filesystem**, and **hardware security modules (HSMs) cannot be reliably accessed from inside containers**.

**Traditional approach (broken):**
- Sign files directly in container ❌ Cannot access TPM/YubiKey
- Sign files on host ❌ Cannot access Docker volume files

**Multi-pass solution:**
- ✅ Images stay in Docker volume (never copied out)
- ✅ Checksums generated using standard container tools (sha256sum/b3sum)
- ✅ Only checksum files are extracted (small, fast)
- ✅ Signing happens on host with full hardware access
- ✅ Signatures are written back via docker cp

### Implementation Details

1. **Checksum Generation**: 
   - Uses standard container utilities (`sha256sum` or `b3sum`)
   - Only checksums `.raw` files in `runtimes/<runtime_name>/extensions/` directory
   - Only checksums extensions that are dependencies of the runtime being built
   - Skips files that already have checksum files to avoid recursive checksumming
   - Saves checksums as `.sha256` or `.blake3` files next to each image

2. **Extension Filtering**:
   - The build process determines which extensions are required for the runtime
   - Only those extensions' `.raw` images are checksummed and signed
   - This prevents signing unnecessary files or creating recursive `.sha256.sha256` files

3. **Checksum Manifest Format**:
```json
{
  "runtime": "production",
  "checksum_algorithm": "blake3",
  "files": [
    {
      "container_path": "/opt/_avocado/qemuarm64/runtimes/production/extensions/bootfiles.raw",
      "hash": "abc123...",
      "size": 1048576
    }
  ]
}
```

3. **Checksum Files**: Standard format checksum files are created:
   - `.sha256` files for SHA-256 checksums
   - `.blake3` files for BLAKE3 checksums

4. **Signature Writing**: Uses `docker cp` to copy signatures into a temporary container with the volume mounted, then the signatures are in the correct location

### Signature File Format

Signature files are JSON format containing:

```json
{
  "version": "1",
  "checksum_algorithm": "sha256",
  "checksum": "abc123...",
  "signature": "def456...",
  "key_name": "production-key",
  "keyid": "sha256-abc123def456"
}
```

### Signed Files

The following files are signed during runtime builds:
- **Extension images only**: `$AVOCADO_PREFIX/<target>/runtimes/<runtime_name>/extensions/*.raw`
  - Only extensions that are dependencies of the runtime being built are signed
  - Each extension's `.raw` image file gets a corresponding `.sig` signature file
  - Extensions are copied from `output/extensions/` to the runtime-specific directory during build

Where:
- `<target>` is the target architecture (e.g., `qemuarm64`, `x86_64-unknown-linux-gnu`)
- `<runtime_name>` is the name of the runtime being built (e.g., `production`, `staging`)

**Note**: Currently, only extension `.raw` images are signed. Stone-generated runtime images and var images are not signed in this version.

## Build Commands

Signing is automatically applied when using:

```bash
# Build a specific runtime
avocado runtime build -r production

# Build all runtimes
avocado build

# Build a specific runtime from the general build command
avocado build -r production
```

Example output:
```
Building runtime images for 'production'
Signing runtime images with key 'production-key' using blake3 checksums
Signed 3 image file(s)
Successfully built runtime 'production'
```

## Security Considerations

1. **Images Never Leave Volume**: Images are never copied to the host; only cryptographic hashes are extracted
2. **Hardware Key Support**: PKCS#11 keys (TPM/YubiKey) are accessed directly on the host where hardware is available
3. **File-Based Keys**: Stored in platform-specific secure locations with 0600 permissions (owner read/write only)
4. **Minimal Container Privileges**: Hash generation and signature writing containers require no special privileges
5. **Read-Only Volume Mount**: The hash generation container mounts the volume as read-only

## PKCS#11 Support Status

- **Key Registration**: ✅ PKCS#11 URIs can be registered via `avocado signing-keys create --uri`
- **Key Generation**: ✅ Generate keys in TPM, YubiKey via `--pkcs11-device` and `--generate`
- **Key Reference**: ✅ Reference existing hardware keys via `--key-label`
- **Key Listing**: ✅ PKCS#11 keys are listed and displayed
- **Signing Operations**: ✅ PKCS#11 signing fully implemented and functional
- **Supported Devices**: ✅ TPM 2.0, YubiKey, and any PKCS#11-compatible device

### How It Works

- **Key Creation/Generation**: Happens on the host with direct access to hardware device
- **Signing**: Occurs on the host (Pass 3 of multi-pass workflow) with direct hardware access
- **No Container Passthrough**: Hardware devices stay on host, only checksums are extracted from containers
- **Algorithm Detection**: Automatically detects key algorithm from device
- **PIN Management**: Flexible authentication (prompt, environment variable, or none)
