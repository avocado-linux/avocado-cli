# YubiKey Testing Guide

This guide walks through testing the PKCS#11 integration with a physical YubiKey.

## Prerequisites

You have OpenSC installed (`/usr/lib/x86_64-linux-gnu/opensc-pkcs11.so`), which can interface with YubiKey PIV applets.

## Step 1: Check YubiKey Detection

First, verify your YubiKey is detected by the system:

```bash
# Check if YubiKey is detected via USB
lsusb | grep -i yubico

# List PKCS#11 tokens (should show YubiKey PIV token)
pkcs11-tool --module /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so --list-token-slots
```

Expected output should show something like:
```
Available slots:
Slot 0 (0x0): Yubico YubiKey OTP+FIDO+CCID 00 00
  token label        : YubiKey PIV #XXXXXXXX
  token manufacturer : Yubico (www.yubico.com)
  ...
```

## Step 2: Initialize YubiKey PIV (If Needed)

If this is a fresh YubiKey or PIV hasn't been initialized:

```bash
# Install YubiKey manager (optional, for easier management)
sudo apt install yubikey-manager

# Check YubiKey info
ykman info

# The PIV applet should already be available by default
# Default PIN is 123456, default PUK is 12345678
```

**Important**: YubiKey PIV slots:
- Slot 9a: Authentication
- Slot 9c: Digital Signature (recommended for signing keys)
- Slot 9d: Key Management
- Slot 9e: Card Authentication

## Step 3: Test Creating a Key in YubiKey

### Option A: Generate New Key in YubiKey

```bash
# Generate a new ECC P-256 key in the YubiKey
avocado signing-keys create my-yubikey-key \
  --pkcs11-device yubikey \
  --token "YubiKey PIV #XXXXXXXX" \
  --generate \
  --auth prompt
```

When prompted, enter your YubiKey PIV PIN (default: `123456`).

**Note**: Replace `YubiKey PIV #XXXXXXXX` with your actual token label from Step 1.

### Option B: Reference Existing Key

If you already have a key in the YubiKey:

```bash
# Reference an existing key by label
avocado signing-keys create my-existing-yk-key \
  --pkcs11-device yubikey \
  --token "YubiKey PIV #XXXXXXXX" \
  --key-label "SIGN key" \
  --auth prompt
```

## Step 4: List Registered Keys

```bash
# List all signing keys registered with avocado
avocado signing-keys list
```

Expected output:
```
Registered signing keys:

  my-yubikey-key
    Key ID:    sha256-abcdef123456
    Algorithm: ecdsa-p256
    Type:      pkcs11
    Created:   2025-12-18 00:45:00 UTC
```

## Step 5: Test Signing (Optional)

If you want to test that the YubiKey key actually works for signing:

```bash
# You would use this key when signing an image
# This will prompt for your YubiKey PIN when signing
```

## Step 6: Remove Key from Registry

### Remove by Name

```bash
avocado signing-keys remove my-yubikey-key
```

Expected output:
```
Removed signing key 'my-yubikey-key'
  Key ID: sha256-abcdef123456
  Note: PKCS#11 key reference removed (hardware key unchanged)
```

### Remove by Key ID

```bash
avocado signing-keys remove sha256-abcdef123456
```

**Important**: Removing the key from avocado's registry only removes the reference. The actual key remains in your YubiKey hardware.

## Step 7: Verify Removal

```bash
# List keys - should not show the removed key
avocado signing-keys list

# Verify key still exists in YubiKey hardware
pkcs11-tool --module /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so \
  --list-objects --login --pin 123456
```

## Troubleshooting

### YubiKey Not Detected

```bash
# Check USB connection
lsusb | grep -i yubico

# Check if pcscd is running (needed for smart cards)
sudo systemctl status pcscd
sudo systemctl start pcscd
```

### "No matching token found"

The token name must match exactly. List available tokens:

```bash
pkcs11-tool --module /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so --list-token-slots
```

Use the exact token label shown (e.g., `YubiKey PIV #12345678`).

### "PIN incorrect" or Authentication Fails

- Default PIV PIN: `123456`
- Default PIV PUK: `12345678`
- After 3 failed PIN attempts, the PIV applet locks
- You can unlock with PUK or reset the PIV applet (WARNING: erases all keys!)

```bash
# Reset PIV applet (DESTRUCTIVE - erases all keys!)
ykman piv reset
```

### Using Environment Variable for PIN

```bash
# Set PIN in environment variable
export AVOCADO_PKCS11_PIN=123456

# Create key without prompting
avocado signing-keys create my-yk-key \
  --pkcs11-device yubikey \
  --token "YubiKey PIV #XXXXXXXX" \
  --generate \
  --auth env
```

## Testing Checklist

- [ ] YubiKey detected via `lsusb`
- [ ] Token visible via `pkcs11-tool`
- [ ] Can create new key in YubiKey with `--generate`
- [ ] Can reference existing key with `--key-label`
- [ ] Key appears in `avocado signing-keys list`
- [ ] Can remove key by name
- [ ] Can remove key by key ID
- [ ] Key removed from registry but still in YubiKey hardware

## Key Differences: TPM vs YubiKey

| Feature | TPM | YubiKey |
|---------|-----|---------|
| Library | `libtpm2_pkcs11.so` | `opensc-pkcs11.so` or `libykcs11.so` |
| Token Label | Often "avocado" or custom | "YubiKey PIV #XXXXXXXX" |
| Default PIN | Custom (set during init) | `123456` |
| Key Storage | Unlimited (limited by TPM memory) | 4 PIV slots (9a, 9c, 9d, 9e) |
| Portability | Machine-bound | Portable (USB device) |

## Advanced: Using Specific YubiKey PIV Slots

YubiKey PIV has specific slots for different purposes. By default, avocado uses the label you provide, but you can target specific slots:

```bash
# Generate key in PIV slot 9c (Digital Signature)
# The slot is determined by the key label in some PKCS#11 implementations
# For OpenSC, you might need to pre-create keys with specific labels

# List all objects in YubiKey
pkcs11-tool --module /usr/lib/x86_64-linux-gnu/opensc-pkcs11.so \
  --list-objects --login --pin 123456
```

## Support

For issues specific to:
- **YubiKey hardware**: https://support.yubico.com/
- **OpenSC**: https://github.com/OpenSC/OpenSC/wiki
- **Avocado CLI**: Check project documentation or file an issue
