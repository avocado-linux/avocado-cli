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

# Require the SDK vars before touching the tree or deriving any path. Letting
# them expand empty would collapse CROSS_BINDIR to "/usr/bin", where the -x check
# below finds the *host* gcc and quietly produces a native binary packaged as a
# target extension; an empty SDKTARGETSYSROOT would bake a bogus --sysroot into
# .cargo/config.toml. This script runs with `set -e` but not `set -u`, so nothing
# else catches it. Guarding here also means neither abort leaves a half-written
# .cargo/config.toml behind.
: "${OECORE_NATIVE_SYSROOT:?not set -- SDK environment-setup was not sourced}"
: "${SDKTARGETSYSROOT:?not set -- SDK environment-setup was not sourced}"
: "${CROSS_COMPILE:?not set -- SDK environment-setup was not sourced}"

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
# --sysroot), so the `cc` crate (pulled in by the C dep aws-lc-sys) needs that
# binary on PATH or it fails with ToolNotFound.
#
# It lives in the cross-canadian bindir under the SDK *native* sysroot -- NOT in
# $SDKTARGETSYSROOT/usr/bin, which holds the target-*native* toolchain. Putting
# the target sysroot bindir on PATH resolves "<triple>-gcc" to a target ELF;
# binfmt_misc then hands it to qemu-user, which dies on the unresolvable target
# loader ("qemu-aarch64: Could not open '/usr/lib/ld-linux-aarch64.so.1'") and
# fails every compiler probe with exit 255. That is invisible on a same-arch
# target like qemux86-64, where the target gcc happens to run natively.
#
# Arch-agnostic: the triple comes from $CROSS_COMPILE, no hardcoded value.
CROSS_BINDIR="$OECORE_NATIVE_SYSROOT/usr/bin/${CROSS_COMPILE%-}"

# Check the binary `cc` will actually run: it takes the FIRST token of $CC (the
# rest are target flags) and falls back to guessing "<triple>-gcc" when $CC is
# unset. Hardcoding gcc here would also abort a working clang SDK.
CC_BIN="${CC:-${CROSS_COMPILE}gcc}"
CC_BIN="${CC_BIN%% *}"
if [ ! -x "$CROSS_BINDIR/$CC_BIN" ]; then
    echo "Error: cross compiler '$CC_BIN' not found in $CROSS_BINDIR" >&2
    echo "The SDK is missing the C cross-canadian toolchain. An SDK installed" >&2
    echo "before it was added to avocado.yaml will not have it -- reinstall with:" >&2
    echo "    avocado sdk install --force" >&2
    exit 1
fi
export PATH="$CROSS_BINDIR:$PATH"

# --locked: published builds run from staged package_files; fail loudly on a
# missing/stale Cargo.lock instead of silently re-resolving dependencies.
cargo build --locked --release --target "$RUST_TARGET"
