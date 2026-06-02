#!/bin/bash
set -e

echo "Cleaning avocado-cli build artifacts"

cd "$(dirname "$0")"

# Remove Cargo build artifacts
cargo clean

# Remove any generated config
rm -rf .cargo

echo "Clean complete"
