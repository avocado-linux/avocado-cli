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

# Register a hardware-backed PKCS#11 key
avocado signing-keys create yubikey-signing --uri "pkcs11:token=YubiKey;object=signing-key"
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
│    each image in output/extensions/                        │
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
   - Only checksums `.raw` files in `output/extensions/` directory
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
      "container_path": "/opt/_avocado/qemuarm64/output/extensions/bootfiles.raw",
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
- **Extension images only**: `$AVOCADO_PREFIX/<target>/output/extensions/*.raw`
  - Only extensions that are dependencies of the runtime being built are signed
  - Each extension's `.raw` image file gets a corresponding `.sig` signature file

Where `<target>` is the target architecture (e.g., `qemuarm64`, `x86_64-unknown-linux-gnu`).

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
- **Key Listing**: ✅ PKCS#11 keys are listed and displayed
- **Signing Operations**: ⚠️  PKCS#11 signing support is planned but not yet implemented
- **Workaround**: Currently, only file-based keys (ed25519) can be used for actual signing

When PKCS#11 support is fully implemented:
- Signing will occur on the host (Pass 2 of multi-pass workflow)
- Hardware devices (TPM, YubiKey) will be accessed directly
- No container privileges or device passthrough required
