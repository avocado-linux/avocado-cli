#!/bin/bash
set -e

# Find the Rust target from RUST_TARGET_PATH
for json_file in "$RUST_TARGET_PATH"/*.json; do
    if [ -f "$json_file" ]; then
        json_name=$(basename "$json_file" .json)
        if [[ "$json_name" == "${OECORE_TARGET_ARCH}-"* ]]; then
            RUST_TARGET="$json_name"
            break
        fi
    fi
done

if [ -z "$RUST_TARGET" ]; then
    echo "Error: Could not find Rust target for $OECORE_TARGET_ARCH"
    exit 1
fi

echo "Building avocado-cli for target: $RUST_TARGET"

cd "$(dirname "$0")"

# Clear any rustflags that might cause conflicts with our .cargo/config.toml.
# The SDK env exports CARGO_TARGET_<triple>_RUSTFLAGS carrying its own --sysroot;
# left set, cargo merges it with the config below and rustc gets --sysroot twice
# ("Option 'sysroot' given more than once"). Unset every target's flavor, not just
# one hardcoded triple, so this works for x86_64 and aarch64 targets alike.
unset RUSTFLAGS
unset CARGO_BUILD_RUSTFLAGS
for var in $(env | grep -o 'CARGO_TARGET_[A-Z0-9_]*_RUSTFLAGS'); do
    unset "$var"
done

# Remove only the generated cross-compile config, preserving any committed
# .cargo files used for development.
rm -f .cargo/config.toml

# Create config.toml with cross-compilation settings
mkdir -p .cargo
cat > .cargo/config.toml << EOF
[target.$RUST_TARGET]
rustflags = ["--sysroot=$SDKTARGETSYSROOT/usr", "-C", "link-arg=--sysroot=$SDKTARGETSYSROOT"]
EOF

# The SDK exports $CC as the cross-compiler command (bare name + target flags +
# --sysroot), but in the `ext build` environment that compiler binary lives in
# the SDK target-sysroot bindir, which is not on PATH. Without this, the `cc`
# crate (pulled in by the remaining C dep aws-lc-sys) can't resolve the compiler
# from $CC and falls back to guessing "<triple>-gcc", failing with ToolNotFound.
# Put the SDK compiler bindir on PATH so the build uses exactly the $CC the SDK
# configured. Arch-agnostic: derived from $SDKTARGETSYSROOT, no hardcoded triple.
export PATH="$SDKTARGETSYSROOT/usr/bin:$PATH"

# --locked: published builds run from staged package_files; fail loudly on a
# missing/stale Cargo.lock instead of silently re-resolving dependencies.
cargo build --locked --release --target "$RUST_TARGET"
