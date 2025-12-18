# Quick Reference: Binary Signing in Provision Scripts

## Setup (in avocado.yaml)

```yaml
signing_keys:
  my-key: my-key-id

runtime:
  my-runtime:
    signing:
      key: my-key
      checksum_algorithm: sha256
```

## Usage (in provision script)

### Simple Usage
```bash
avocado-sign-request /opt/_avocado/x86_64/runtimes/my-runtime/binary.bin
```

### With Error Handling
```bash
if avocado-sign-request /path/to/binary; then
    echo "Signed successfully"
else
    echo "Signing failed"
    exit 1
fi
```

### Check Availability
```bash
if [ -n "$AVOCADO_SIGNING_ENABLED" ]; then
    avocado-sign-request /path/to/binary
fi
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Signing failed |
| 2 | Signing unavailable |
| 3 | File not found |

## Environment Variables

### General Variables
- `$AVOCADO_RUNTIME_BUILD_DIR` - Full path to runtime build directory (e.g., `/opt/_avocado/x86_64/runtimes/<runtime-name>`)
- `$AVOCADO_EXT_LIST` - Space-separated list of required extensions
- `$AVOCADO_PROVISION_OUT` - Output directory (if `--out` specified). File ownership automatically fixed to calling user.
- `$AVOCADO_PROVISION_STATE` - Path to state file for persisting state between provision runs (if `--provision-profile` specified). See State File section below.
- `$AVOCADO_STONE_INCLUDE_PATHS` - Stone include paths (if configured)
- `$AVOCADO_STONE_MANIFEST` - Stone manifest path (if configured)

### Signing Variables
- `$AVOCADO_SIGNING_ENABLED` - Set to "1" when available
- `$AVOCADO_SIGNING_KEY_NAME` - Key name being used
- `$AVOCADO_SIGNING_CHECKSUM` - Algorithm (sha256/blake3)
- `$AVOCADO_SIGNING_SOCKET` - Socket path

## Output

Creates `{binary}.sig` file next to the binary:

```
/opt/_avocado/x86_64/runtimes/my-runtime/
├── binary.bin
└── binary.bin.sig  ← Created by signing
```

## Path Requirements

Binary must be in one of these locations:
- `/opt/_avocado/{target}/runtimes/{runtime}/...`
- `/opt/_avocado/{target}/output/runtimes/{runtime}/...`

❌ Won't work: `/tmp/binary`, `/opt/src/binary`  
✅ Will work: 
- `/opt/_avocado/x86_64/runtimes/my-runtime/binary`
- `/opt/_avocado/x86_64/output/runtimes/my-runtime/binary`

## Complete Example

```bash
#!/bin/bash
# avocado-provision-x86_64

set -e

# Use the runtime build directory variable
RUNTIME_DIR="$AVOCADO_RUNTIME_BUILD_DIR"

# Build binary
make firmware.bin

# Copy to runtime directory
cp firmware.bin "$RUNTIME_DIR/"

# Sign it
if command -v avocado-sign-request &> /dev/null; then
    if avocado-sign-request "$RUNTIME_DIR/firmware.bin"; then
        echo "✓ Signed firmware.bin"
    else
        echo "✗ Failed to sign firmware.bin"
        exit 1
    fi
fi

# Continue provisioning...
```

## Troubleshooting

**Socket not available?**
- Check signing is configured in avocado.yaml
- Check signing key exists: `avocado signing-keys list`

**Path validation error?**
- Ensure binary is in `/opt/_avocado/{target}/runtimes/{runtime}/`
- No `..` in path

**Timeout?**
- Check binary size (signing takes longer for large files)
- Default timeout is 30 seconds

## State File

When using a provision profile (`--provision-profile`), you can persist state between provision runs using a JSON state file.

### Configuration

```yaml
provision:
  production:
    state_file: my-state.json  # Optional, defaults to provision-{profile}.json
    container_args:
      - --privileged
```

### Usage

```bash
#!/bin/bash
# In your provision script

if [ -f "$AVOCADO_PROVISION_STATE" ]; then
    echo "Previous state exists, reading..."
    DEVICE_ID=$(jq -r '.device_id' "$AVOCADO_PROVISION_STATE")
else
    echo "First run, creating state..."
    DEVICE_ID=$(uuidgen)
fi

# Save state for next run
jq -n --arg id "$DEVICE_ID" '{"device_id": $id}' > "$AVOCADO_PROVISION_STATE"
```

### How It Works

1. Before provisioning: If `<src_dir>/<state_file>` exists, it's copied into the container
2. During provisioning: Script can read/modify `$AVOCADO_PROVISION_STATE`
3. After provisioning: If the file exists in the container, it's copied back to `<src_dir>/<state_file>` with correct ownership

## More Information

See [`docs/provision-signing.md`](provision-signing.md) for complete documentation.

