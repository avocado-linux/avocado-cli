# Integration Tests for PKCS#11/TPM Support

## Running Tests Locally

### Automated Tests (Software TPM)

The integration tests automatically start a software TPM simulator (swtpm) for testing:

```bash
# Install prerequisites
sudo apt install swtpm swtpm-tools libtpm2-pkcs11-1 tpm2-tools

# Run the tests
cargo test --test pkcs11_integration_test
```

### What the Tests Do

1. **Automatically start swtpm** - A software TPM simulator in the background
2. **Initialize the TPM** - Set up the simulated TPM device
3. **Test key generation** - Generate ECC P-256 keys in the TPM
4. **Test signing** - Sign data using TPM-backed keys
5. **Test end-to-end workflow** - Complete key lifecycle
6. **Test key registration and removal** - Register TPM keys with avocado and remove them by name or key ID
7. **Cleanup** - Automatically stop swtpm and remove temporary files

### Test Output

```
running 11 tests
test test_auth_method_parsing ... ok
test test_device_type_parsing ... ok
test test_keyid_generation ... ok
test test_pkcs11_uri_building ... ok
test test_pkcs11_uri_parsing ... ok
test test_tpm_connection ... ok
test test_tpm_key_generation ... ok
test test_tpm_signing ... ok
test test_end_to_end_tpm_workflow ... ok
test test_tpm_key_registration_and_removal ... ok
```

If swtpm is not available, tests will be skipped with a helpful message.

## CI/CD Integration

### GitHub Actions Example

```yaml
- name: Install TPM Testing Tools
  run: |
    sudo apt-get update
    sudo apt-get install -y swtpm swtpm-tools libtpm2-pkcs11-1 tpm2-tools

- name: Run PKCS#11 Integration Tests
  run: cargo test --test pkcs11_integration_test
```

### GitLab CI Example

```yaml
test:pkcs11:
  stage: test
  before_script:
    - apt-get update
    - apt-get install -y swtpm swtpm-tools libtpm2-pkcs11-1 tpm2-tools
  script:
    - cargo test --test pkcs11_integration_test
```

## Testing with Real Hardware

To test with actual TPM or YubiKey hardware:

### Physical TPM

```bash
# Install TPM packages
sudo apt install libtpm2-pkcs11-1 tpm2-tools

# Add your user to the TPM group
sudo usermod -a -G tss $USER

# Log out and back in, then test
avocado signing-keys create my-tpm-key --pkcs11-device tpm --generate --auth none
```

### YubiKey

```bash
# Install YubiKey packages
sudo apt install yubikey-manager libykcs11-1

# Insert YubiKey, then test
avocado signing-keys create my-yk-key --pkcs11-device yubikey --generate --auth prompt
```

## Troubleshooting

### swtpm not found

```bash
sudo apt install swtpm swtpm-tools
```

### TPM PKCS#11 library not found

```bash
sudo apt install libtpm2-pkcs11-1
```

### Tests are skipped

If tests are skipped, check that both swtpm and libtpm2-pkcs11-1 are installed:

```bash
which swtpm
dpkg -l | grep libtpm2-pkcs11
```

### Permission denied errors

When testing with real TPM hardware, ensure your user is in the `tss` group:

```bash
sudo usermod -a -G tss $USER
newgrp tss
```

