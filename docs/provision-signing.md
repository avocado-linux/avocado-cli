# Binary Signing During Provisioning

This document describes how target-specific provision scripts can request binary signing from the host CLI during `avocado provision` execution.

## Overview

The Avocado CLI provides a mechanism for provision scripts running inside containers to request binary signing from the host without breaking script execution flow. This is accomplished using Unix domain sockets for bidirectional communication.

## Architecture

When you run `avocado provision` for a runtime that has signing configured:

1. The host CLI starts a signing service listening on a Unix socket
2. The socket and a helper script are mounted into the container
3. Provision scripts can call `avocado-sign-request` to request binary signing
4. The host signs the binary using the configured key
5. The signature is written back to the volume
6. The script continues execution

## Configuration

To enable signing during provisioning, configure a signing key for your runtime in `avocado.yaml`:

```yaml
signing_keys:
  my-key: my-key-id

runtime:
  my-runtime:
    signing:
      key: my-key
      checksum_algorithm: sha256  # or blake3
```

## Usage in Provision Scripts

### Basic Example

```bash
#!/bin/bash
# avocado-provision-x86_64 script

set -e

# Generate a custom binary
echo "Building custom bootloader..."
make -C /opt/src/bootloader custom-bootloader.bin

# Copy to runtime directory
cp /opt/src/bootloader/custom-bootloader.bin \
   /opt/_avocado/x86_64/runtimes/my-runtime/custom-bootloader.bin

# Request signing from host
if command -v avocado-sign-request &> /dev/null; then
    echo "Requesting signature from host..."
    if avocado-sign-request /opt/_avocado/x86_64/runtimes/my-runtime/custom-bootloader.bin; then
        echo "Binary signed successfully"
    else
        echo "Error: Failed to sign binary"
        exit 1
    fi
else
    echo "Warning: Signing not available"
fi

# Continue with provisioning...
```

### Checking Signing Availability

```bash
# Check if signing is enabled
if [ -n "$AVOCADO_SIGNING_ENABLED" ]; then
    echo "Signing is available"
    echo "Using key: $AVOCADO_SIGNING_KEY_NAME"
    echo "Algorithm: $AVOCADO_SIGNING_CHECKSUM"
fi
```

### Error Handling

The `avocado-sign-request` helper script returns different exit codes:

- `0`: Success - binary was signed
- `1`: Signing failed - there was an error during signing
- `2`: Signing unavailable - socket not available
- `3`: File not found - binary doesn't exist

Example error handling:

```bash
if ! avocado-sign-request /path/to/binary; then
    EXIT_CODE=$?
    case $EXIT_CODE in
        1)
            echo "Error: Signing failed"
            exit 1
            ;;
        2)
            echo "Warning: Signing not available, continuing anyway"
            ;;
        3)
            echo "Error: Binary not found"
            exit 1
            ;;
    esac
fi
```

## Environment Variables

### General Environment Variables

The following environment variables are available in provision and build scripts:

- `AVOCADO_RUNTIME_BUILD_DIR`: Full path to the runtime build directory (e.g., `/opt/_avocado/x86_64/runtimes/<runtime-name>`)
- `AVOCADO_EXT_LIST`: Space-separated list of extensions required by the runtime (if any)
- `AVOCADO_PROVISION_OUT`: Output directory path in the container (if `--out` flag is specified)
- `AVOCADO_STONE_INCLUDE_PATHS`: Stone include paths (if configured for the runtime)
- `AVOCADO_STONE_MANIFEST`: Stone manifest path (if configured for the runtime)

### Signing-Related Environment Variables

The following environment variables are available in the container when signing is enabled:

- `AVOCADO_SIGNING_ENABLED`: Set to `1` when signing is available
- `AVOCADO_SIGNING_SOCKET`: Path to the signing socket (`/run/avocado/sign.sock`)
- `AVOCADO_SIGNING_KEY_NAME`: Name of the signing key being used
- `AVOCADO_SIGNING_CHECKSUM`: Checksum algorithm (`sha256` or `blake3`)

## Signature Files

When a binary is successfully signed, a signature file is created with the `.sig` extension:

```
/opt/_avocado/x86_64/runtimes/my-runtime/
├── custom-bootloader.bin
└── custom-bootloader.bin.sig
```

The signature file contains JSON with the following structure:

```json
{
  "version": "1",
  "checksum_algorithm": "sha256",
  "checksum": "a1b2c3...",
  "signature": "d4e5f6...",
  "key_name": "my-key",
  "keyid": "my-key-id"
}
```

## Communication Protocol

The signing protocol uses line-delimited JSON over Unix domain sockets.

### Request Format

```json
{
  "type": "sign_request",
  "binary_path": "/opt/_avocado/x86_64/runtimes/my-runtime/custom-binary",
  "checksum_algorithm": "sha256"
}
```

### Response Format

Success:
```json
{
  "type": "sign_response",
  "success": true,
  "signature_path": "/opt/_avocado/x86_64/runtimes/my-runtime/custom-binary.sig",
  "signature_content": "{ ... signature JSON ... }",
  "error": null
}
```

Error:
```json
{
  "type": "sign_response",
  "success": false,
  "signature_path": null,
  "signature_content": null,
  "error": "Error message here"
}
```

## Security

- **Path Validation**: Only binaries within the runtime's volume path can be signed
- **Socket Permissions**: Socket file has 0600 permissions (owner only)
- **Read-only Keys**: Signing keys are never exposed to the container
- **No Direct Access**: All signing operations happen on the host

## Limitations

- Binary must exist in one of the runtime's directory structures:
  - `/opt/_avocado/{target}/runtimes/{runtime}/...`
  - `/opt/_avocado/{target}/output/runtimes/{runtime}/...`
- Path traversal (`..`) is not allowed
- Socket operations have a 30-second timeout
- Only one signing operation can be processed at a time per container

## Troubleshooting

### "Error: Signing socket not available"

The signing service is not running. This can happen if:
- No signing key is configured for the runtime
- The socket mount failed
- The signing service failed to start

Check the host CLI output for errors during provision startup.

### "Warning: avocado-sign-request not available"

The helper script was not mounted properly. This should not happen in normal operation. If you see this:
- Ensure you're using the latest version of avocado-cli
- Check that the signing service started successfully (you should see a message about "Starting signing service")
- Try running with `--verbose` flag to see detailed mount information

### "Error: Binary not found"

The binary path doesn't exist. Make sure:
- The binary was created successfully
- The path is absolute
- The path points to the correct location in the volume

### "Error signing binary: Binary path is not within expected runtime directory"

The binary path must be within one of the allowed runtime directories:
- `/opt/_avocado/{target}/runtimes/{runtime}/...`
- `/opt/_avocado/{target}/output/runtimes/{runtime}/...`

You cannot sign binaries outside these directories for security reasons.

**Valid examples:**
- `/opt/_avocado/x86_64/runtimes/my-runtime/firmware.bin`
- `/opt/_avocado/x86_64/output/runtimes/my-runtime/_build/firmware.bin`

**Invalid examples:**
- `/tmp/firmware.bin` (not in runtime directory)
- `/opt/src/firmware.bin` (source directory, not volume)
- `/opt/_avocado/x86_64/runtimes/other-runtime/binary` (wrong runtime)

## Example: Complete Provisioning Workflow

```bash
#!/bin/bash
# avocado-provision-x86_64 script for custom hardware

set -e

RUNTIME_DIR="/opt/_avocado/x86_64/runtimes/my-hardware"

echo "Building firmware for my-hardware..."
cd /opt/src/firmware
make clean
make ARCH=x86_64

echo "Copying firmware to runtime directory..."
cp build/firmware.bin "$RUNTIME_DIR/firmware.bin"
cp build/bootloader.bin "$RUNTIME_DIR/bootloader.bin"

echo "Signing firmware components..."
for binary in firmware.bin bootloader.bin; do
    if avocado-sign-request "$RUNTIME_DIR/$binary"; then
        echo "✓ Signed $binary"
    else
        echo "✗ Failed to sign $binary"
        exit 1
    fi
done

echo "Creating provisioning manifest..."
cat > "$RUNTIME_DIR/manifest.json" <<EOF
{
  "firmware": {
    "file": "firmware.bin",
    "signature": "firmware.bin.sig",
    "version": "1.0.0"
  },
  "bootloader": {
    "file": "bootloader.bin",
    "signature": "bootloader.bin.sig",
    "version": "2.0.0"
  }
}
EOF

echo "Provisioning complete!"
```

## See Also

- [Signing Keys Documentation](signing-keys.md) - Managing signing keys
- [Runtime Configuration](../README.md#runtime-configuration) - Configuring runtimes
- [Extension Signing](extension-signing.md) - Signing extension images

