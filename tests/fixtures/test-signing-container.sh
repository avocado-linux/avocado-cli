#!/bin/bash
# Test script to verify signing keys are mounted in container

set -e

echo "=== Testing Signing Keys Container Mount ==="
echo

# Check if AVOCADO_SIGNING_KEYS_DIR is set
if [ -n "$AVOCADO_SIGNING_KEYS_DIR" ]; then
    echo "✓ AVOCADO_SIGNING_KEYS_DIR is set: $AVOCADO_SIGNING_KEYS_DIR"
else
    echo "✗ AVOCADO_SIGNING_KEYS_DIR is not set"
    exit 1
fi

# Check if the directory exists
if [ -d "$AVOCADO_SIGNING_KEYS_DIR" ]; then
    echo "✓ Signing keys directory exists"
else
    echo "✗ Signing keys directory does not exist"
    exit 1
fi

# Check if we can read the directory
if [ -r "$AVOCADO_SIGNING_KEYS_DIR" ]; then
    echo "✓ Signing keys directory is readable"
else
    echo "✗ Signing keys directory is not readable"
    exit 1
fi

# List contents (if any)
echo
echo "Directory contents:"
ls -la "$AVOCADO_SIGNING_KEYS_DIR" || echo "(empty or no access)"

echo
echo "✓ All container mount tests passed!"
