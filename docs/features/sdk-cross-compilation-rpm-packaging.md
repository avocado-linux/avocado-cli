# SDK Cross-Compilation RPM Packaging

## Context

avocado-cli can already cross-compile code via `sdk compile` and package extension source directories via `ext package`. However, there's no way to package **compiled artifacts** into RPMs for publishing to private RPM repositories. Developers need a `sdk package` command that takes cross-compiled output, stages it into a sysroot layout, and creates proper architecture-specific RPMs with optional Yocto-inspired sub-package splitting (-dev, -dbg, -src).

## Config Schema

Extend `sdk.compile` sections with a `package` block containing `install` (staging script) and RPM config:

```yaml
sdk:
  compile:
    my-app:
      compile: scripts/compile.sh      # existing - cross-compiles code
      clean: scripts/clean.sh          # existing
      packages:                        # existing - build dependencies
        gcc-aarch64-linux-gnu: "*"

      package:                         # NEW - RPM packaging config
        install: scripts/install.sh    # stages files to $DESTDIR
        version: "1.0.0"              # required (semver)
        name: my-app                   # defaults to section name
        release: "1"                   # defaults to "1"
        license: "MIT"                 # defaults to "Unspecified"
        summary: "My application"      # auto-generated if missing
        vendor: "Acme Corp"            # defaults to "Unspecified"
        url: "https://..."             # optional
        arch: "aarch64"               # defaults to target-derived RPM arch; override if needed
        requires:                      # RPM Requires: dependencies
          - "glibc >= 2.17"

        # Omit `files` -> all staged files go in one RPM
        # Specify `files` -> only matching files in main RPM
        files:
          - /usr/bin/*
          - /usr/lib/lib*.so.*

        # Sub-packages (Yocto-inspired file selection)
        split:
          dev:
            summary: "Development files for my-app"
            requires:
              - "my-app = 1.0.0"
            files:
              - /usr/include/**
              - /usr/lib/lib*.so
              - /usr/lib/pkgconfig/**
          dbg:
            summary: "Debug symbols for my-app"
            files:
              - /usr/lib/debug/**
              - /usr/lib/.debug/**
```

Minimal config (most common case - single RPM with everything):
```yaml
sdk:
  compile:
    my-app:
      compile: build.sh
      package:
        install: install.sh
        version: "1.0.0"
```

## Developer Workflow

```bash
avocado sdk install                              # install SDK + build deps
avocado sdk compile my-app                       # cross-compile
avocado sdk package my-app --out-dir ./rpms      # stage + package RPM(s)
# Output: ./rpms/my-app-1.0.0-1.aarch64.rpm
#         ./rpms/my-app-dev-1.0.0-1.aarch64.rpm  (if split defined)
```

## Pipeline: `sdk package <section>`

1. Validate SDK install stamp (same as `sdk compile`)
2. Validate compile section has `package` block with `install` script
3. Run `package.install` script with `$DESTDIR=$AVOCADO_SDK_PREFIX/staging/<section>/`
4. If `split` defined, partition staged files by glob patterns (first match wins)
5. Generate RPM spec with `%package -n` for sub-packages
6. Run `rpmbuild --target <rpm-arch>` in container
7. Output to `$AVOCADO_PREFIX/output/packages/` or `--out-dir`

Key difference from `ext package`: packages compiled binaries (arch-specific), not source (noarch).

## Coexistence with Extension Compile References

Extensions already reference `sdk.compile` sections via `extensions.<ext>.packages.<dep>.compile`. Adding a `package` block to a compile section does **not** affect extensions — they ignore it and continue using their own `install` script to copy artifacts into the extension sysroot.

A single compile section can serve both paths simultaneously:

```yaml
sdk:
  compile:
    my-app:
      compile: build.sh               # shared compile step
      package:                         # only used by `sdk package`
        install: install.sh
        version: "1.0.0"

extensions:
  my-ext:
    packages:
      my-app:
        compile: my-app                # reuses sdk.compile.my-app.compile
        install: ext-install.sh        # extension-specific install (separate script)
```

The compile output (`$AVOCADO_BUILD_DIR`) is shared between both paths. The install scripts are independent — `package.install` stages to `$DESTDIR` for RPMs, while the extension install copies to `$AVOCADO_BUILD_EXT_SYSROOT` for sysext/confext.

## Build Directory Convention (`$AVOCADO_BUILD_DIR`)

Today compile scripts have no standard location for build output — artifacts may end up in the source tree. We introduce `$AVOCADO_BUILD_DIR` as a per-section, auto-created build output directory. It's opt-in: scripts can use it or ignore it.

**Location:** `$AVOCADO_SDK_PREFIX/build/<section-name>/`

This env var is set by both `sdk compile` and `sdk package` (and available to extension install scripts too), so all paths share a common location for compiled artifacts.

```bash
# compile.sh — any build system can use it
cmake -B $AVOCADO_BUILD_DIR -S . && cmake --build $AVOCADO_BUILD_DIR
# or: cargo build --target-dir $AVOCADO_BUILD_DIR
# or: make O=$AVOCADO_BUILD_DIR
# or: go build -o $AVOCADO_BUILD_DIR/my-app .
```

**Implementation:** In `sdk/compile.rs`, set `AVOCADO_BUILD_DIR` alongside the existing `AVOCADO_SDK_PREFIX` when invoking scripts (line ~274). The directory is `mkdir -p`'d before the script runs. Same for `sdk/package.rs` when running the install script, and in `ext/build.rs` when running extension install scripts.

## Install Script Convention

The install script receives `$DESTDIR` and copies from `$AVOCADO_BUILD_DIR`:
```bash
#!/bin/bash
# install.sh
mkdir -p $DESTDIR/usr/bin
cp $AVOCADO_BUILD_DIR/my-app $DESTDIR/usr/bin/
mkdir -p $DESTDIR/etc/my-app
cp config.toml $DESTDIR/etc/my-app/
```

Environment variables available to all scripts:
| Variable | Set by | Purpose |
|----------|--------|---------|
| `$AVOCADO_BUILD_DIR` | compile, package, ext build | Per-section build output dir (opt-in) |
| `$AVOCADO_SDK_PREFIX` | entrypoint | SDK toolchains and sysroots |
| `$AVOCADO_TARGET` | entrypoint | Target architecture |
| `$AVOCADO_PREFIX` | entrypoint | Base prefix for target |
| `$DESTDIR` | package only | Staging root for RPM packaging |
| `$AVOCADO_BUILD_EXT_SYSROOT` | ext build only | Extension sysroot destination |

## File Selection Algorithm (for split packages)

For each file in `$DESTDIR`:
1. Check against sub-package patterns in definition order (first match wins)
2. Unmatched files go to main package
3. If `files` specified on main package, only matching files included; others generate warnings
4. If `files` omitted on main package, all unmatched files included
5. Empty sub-packages are skipped with a warning

## RPM Architecture

If `package.arch` is explicitly set in config, use that value directly. Otherwise, derive from target triple:
- `aarch64-*` -> `aarch64`
- `x86_64-*` -> `x86_64`
- `armv7-*` -> `armv7hl`
- `riscv64-*` -> `riscv64`
- `i686-*` -> `i686`

## Implementation Steps

### 1. Config structs (`src/utils/config.rs`)

Add after `CompileConfig` (line ~419):

- `PackageConfig` struct: `install`, `name`, `version`, `release`, `license`, `summary`, `description`, `vendor`, `url`, `arch`, `requires`, `files`, `split` (all Optional except `version` and `install`). `arch` defaults to target-derived RPM arch but can be explicitly set (e.g., `noarch` for pure config packages)
- `SplitPackageConfig` struct: `summary`, `description`, `requires`, `files` (files required)
- Extend `CompileConfig` with `package: Option<PackageConfig>` (no `install` on CompileConfig — it lives inside PackageConfig)

`package` is `Option<T>` on `CompileConfig`, so existing configs deserialize unchanged.

### 2. Add `$AVOCADO_BUILD_DIR` to existing compile/build paths

- **`src/commands/sdk/compile.rs`** (~line 274): When building the compile command string, add `AVOCADO_BUILD_DIR=$AVOCADO_SDK_PREFIX/build/<section-name>` and `mkdir -p` before invoking the script.
- **`src/commands/ext/build.rs`** (~line 1664): When running extension install scripts, also export `AVOCADO_BUILD_DIR` so ext install scripts can find compiled output.

These are small, additive changes to the command strings that already set `AVOCADO_SDK_PREFIX`.

### 3. New command module (`src/commands/sdk/package.rs`)

Core `SdkPackageCommand` struct following `SdkCompileCommand` pattern:
- `section: String` (singular - package one section at a time)
- `output_dir: Option<String>` (follows `ext package` pattern)
- Standard fields: config_path, verbose, target, container_args, dnf_args, no_stamps, sdk_arch

Key methods:
- `execute()` - main pipeline (stamp check -> package.install script -> RPM build)
- `target_to_rpm_arch()` - target triple to RPM arch mapping
- `generate_rpm_build_script()` - shell script for staging + spec generation + rpmbuild
- `extract_rpm_metadata()` - reads from PackageConfig (not raw YAML like ext package)
- `copy_rpm_to_host()` / `create_temp_container()` - docker cp pattern from ext package

RPM spec supports `%package -n <name>-<subpkg>` for sub-packages with separate `%files` sections.

### 4. Module registration (`src/commands/sdk/mod.rs`)

Add `pub mod package;` and `pub use package::SdkPackageCommand;`

### 5. CLI wiring (`src/main.rs`)

- Import `SdkPackageCommand`
- Add `Package` variant to `SdkCommands` enum with args: config, verbose, target, section, output_dir, container_args, dnf_args
- Add dispatch arm following `SdkCommands::Compile` pattern

### 6. Tests

Unit tests in `package.rs`:
- Constructor, builder methods
- `target_to_rpm_arch()` for all supported targets + unknown pass-through
- RPM metadata extraction (minimal, full, missing version error)
- Summary/description auto-generation

Config deserialization tests in `config.rs`:
- CompileConfig with/without new fields (backward compat)
- PackageConfig minimal (just version) and full
- SplitPackageConfig deserialization

## Critical Files

| File | Change |
|------|--------|
| `src/utils/config.rs` | Add `PackageConfig` (with `install`), `SplitPackageConfig`; add `package` to `CompileConfig` |
| `src/commands/sdk/compile.rs` | Add `$AVOCADO_BUILD_DIR` env var to compile command strings |
| `src/commands/ext/build.rs` | Add `$AVOCADO_BUILD_DIR` env var to extension install command strings |
| `src/commands/sdk/package.rs` | **NEW** - `SdkPackageCommand` implementation |
| `src/commands/sdk/mod.rs` | Register + re-export new module |
| `src/main.rs` | Add `Package` to `SdkCommands`, dispatch arm |

Reference files (patterns to follow):
- `src/commands/sdk/compile.rs` - command structure, stamp validation, container execution
- `src/commands/ext/package.rs` - RPM spec generation, docker cp, metadata extraction

## Verification

1. `cargo build` in avocado-cli - confirms compilation
2. `cargo test` - new unit tests pass, existing tests unchanged
3. Manual test with a minimal config:
   ```yaml
   sdk:
     image: docker.io/avocadolinux/sdk:dev
     compile:
       hello:
         compile: compile.sh
         packages:
           gcc: "*"
         package:
           install: install.sh
           version: "1.0.0"
   ```
   ```bash
   avocado sdk compile hello
   avocado sdk package hello --out-dir ./out
   rpm -qip ./out/hello-1.0.0-1.*.rpm  # verify metadata
   ```
4. Test sub-package splitting with a `split` config and verify multiple RPMs are created
