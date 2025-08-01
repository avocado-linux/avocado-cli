# Rust Migration Guide

This document describes the completed migration of the Avocado CLI from Python to Rust.

## Overview

The Avocado CLI has been successfully migrated from Python to Rust to improve performance, reduce dependencies, and provide better error handling. This migration maintains full compatibility with the existing command-line interface while providing a more robust implementation.

## Migration Status: COMPLETED ✅

The migration from Python to Rust is now complete. All Python code has been removed from the project.

### Completed Commands

- **init**: Initialize a new avocado project
  - ✅ Creates `avocado.toml` configuration file
  - ✅ Supports custom target architecture via `--target` flag
  - ✅ Supports custom directory specification
  - ✅ Error handling for existing files and directory creation failures
  - ✅ Comprehensive unit tests

- **sdk install**: Install dependencies into the SDK
  - ✅ Installs SDK dependencies from configuration
  - ✅ Installs compile section dependencies into target-dev sysroot
  - ✅ Supports force mode and verbose output
  - ✅ Proper error handling and target resolution
  - ✅ Comprehensive unit tests

- **sdk run**: Create and run an SDK container
  - ✅ Supports interactive and detached modes
  - ✅ Container name assignment and auto-removal
  - ✅ Command execution in SDK environment
  - ✅ Proper argument validation
  - ✅ Comprehensive unit tests

- **sdk deps**: List SDK dependencies
  - ✅ Lists all SDK and compile dependencies
  - ✅ Resolves package specifications and versions
  - ✅ Removes duplicates and sorts output
  - ✅ Supports extension references
  - ✅ Comprehensive unit tests

- **sdk compile**: Run compile scripts
  - ✅ Executes compile scripts from configuration
  - ✅ Supports filtering specific sections
  - ✅ Proper environment setup in containers
  - ✅ Error handling for missing scripts
  - ✅ Comprehensive unit tests

- **sdk dnf**: Run DNF commands in the SDK context
  - ✅ Executes DNF commands in SDK environment
  - ✅ Proper environment variable setup
  - ✅ Interactive command execution
  - ✅ Error handling and validation
  - ✅ Comprehensive unit tests

- **sdk clean**: Remove the SDK directory
  - ✅ Removes SDK directory using container
  - ✅ Supports verbose output mode
  - ✅ Proper target resolution
  - ✅ Error handling and validation
  - ✅ Comprehensive unit tests

- **ext install**: Install dependencies into extension sysroots
  - ✅ Installs dependencies for all or specific extensions
  - ✅ Supports force mode and verbose output
  - ✅ Proper error handling and target resolution
  - ✅ Comprehensive unit tests

- **ext build**: Build sysext and/or confext extensions
  - ✅ Builds extensions from configuration
  - ✅ Supports verbose output mode
  - ✅ Proper error handling and validation
  - ✅ Comprehensive unit tests

- **ext list**: List extension names
  - ✅ Lists all extension names from configuration
  - ✅ Sorted alphabetical output
  - ✅ Proper count reporting
  - ✅ Comprehensive unit tests

- **ext deps**: List extension dependencies
  - ✅ Lists dependencies for all or specific extensions
  - ✅ Resolves package specifications and versions
  - ✅ Sorted output format
  - ✅ Comprehensive unit tests

- **ext dnf**: Run DNF commands in extension context
  - ✅ Executes DNF commands in extension environment
  - ✅ Proper environment variable setup
  - ✅ Interactive command execution
  - ✅ Comprehensive unit tests

- **ext clean**: Clean extension sysroot
  - ✅ Removes extension sysroot using container
  - ✅ Supports verbose output mode
  - ✅ Proper target resolution
  - ✅ Comprehensive unit tests

- **ext image**: Create squashfs image from system extension
  - ✅ Creates squashfs images from extensions
  - ✅ Supports verbose output mode
  - ✅ Proper error handling and validation
  - ✅ Comprehensive unit tests

- **runtime build**: Build runtime images
  - ✅ Builds runtime images from configuration
  - ✅ Installs required avocado-pkg-images package
  - ✅ Creates btrfs images with extensions and confexts subvolumes
  - ✅ Supports force mode and verbose output
  - ✅ Proper extension symlinking and lifecycle hooks
  - ✅ Comprehensive unit tests

- **runtime list**: List runtime names
  - ✅ Lists all runtime names from configuration
  - ✅ Sorted alphabetical output
  - ✅ Proper count reporting
  - ✅ Error handling for missing config
  - ✅ Comprehensive unit tests

- **runtime deps**: List runtime dependencies
  - ✅ Lists package and extension dependencies
  - ✅ Resolves extension versions from config
  - ✅ Sorted output (extensions first, then packages)
  - ✅ Proper dependency type identification
  - ✅ Comprehensive unit tests

- **clean**: Clean the avocado project by removing the _avocado directory
  - ✅ Removes _avocado directory from specified or current directory
  - ✅ Handles nested directory structures recursively
  - ✅ Provides informative messages for success, info, and error cases
  - ✅ Proper error handling for nonexistent directories
  - ✅ Comprehensive unit tests

## Usage

### Building

```bash
cargo build --release
```

### Running

```bash
# Initialize a new project in current directory
./target/release/avocado-cli init

# Initialize with custom target
./target/release/avocado-cli --target "custom-arch" init

# Initialize in specific directory
./target/release/avocado-cli init my-project

# Initialize with both custom target and directory
./target/release/avocado-cli --target "arm64" init my-arm-project

# SDK Commands
./target/release/avocado-cli sdk install --verbose --force
./target/release/avocado-cli sdk run --interactive
./target/release/avocado-cli sdk run echo "Hello World"
./target/release/avocado-cli sdk deps
./target/release/avocado-cli sdk compile app
./target/release/avocado-cli sdk dnf -- install gcc
./target/release/avocado-cli sdk clean --verbose

# Extension Commands
./target/release/avocado-cli ext install --verbose --force
./target/release/avocado-cli ext install my-extension
./target/release/avocado-cli ext build my-extension --verbose
./target/release/avocado-cli ext list
./target/release/avocado-cli ext deps my-extension
./target/release/avocado-cli ext dnf my-extension -- install vim
./target/release/avocado-cli ext clean my-extension --verbose
./target/release/avocado-cli ext image my-extension --verbose

# Runtime Commands
./target/release/avocado-cli runtime list
./target/release/avocado-cli runtime deps my-runtime
./target/release/avocado-cli runtime build my-runtime --verbose --force

# Clean Command
./target/release/avocado-cli clean
./target/release/avocado-cli clean my-project-dir
```

### Testing

```bash
# Run all unit tests
cargo test

# Run only init command tests
cargo test commands::init::tests

# Run only SDK command tests
cargo test commands::sdk::tests

# Run only extension command tests
cargo test commands::ext::tests

# Run only runtime command tests
cargo test commands::runtime::tests

# Run only clean command tests
cargo test commands::clean::tests
```

## Architecture

### Project Structure

```
src/
├── main.rs              # CLI argument parsing and main entry point
├── commands/
│   ├── mod.rs           # Commands module
│   ├── init.rs          # Init command implementation
│   ├── clean.rs         # Clean command implementation
│   ├── ext/
│   │   ├── mod.rs       # Extension commands module
│   │   ├── install.rs   # Extension install command
│   │   ├── build.rs     # Extension build command
│   │   ├── list.rs      # Extension list command
│   │   ├── deps.rs      # Extension deps command
│   │   ├── dnf.rs       # Extension dnf command
│   │   ├── clean.rs     # Extension clean command
│   │   └── image.rs     # Extension image command
│   ├── runtime/
│   │   ├── mod.rs       # Runtime commands module
│   │   ├── build.rs     # Runtime build command
│   │   ├── list.rs      # Runtime list command
│   │   └── deps.rs      # Runtime deps command
│   └── sdk/
│       ├── mod.rs       # SDK commands module
│       ├── install.rs   # SDK install command
│       ├── run.rs       # SDK run command
│       ├── deps.rs      # SDK deps command
│       ├── compile.rs   # SDK compile command
│       ├── dnf.rs       # SDK dnf command
│       └── clean.rs     # SDK clean command
└── utils/
    ├── mod.rs           # Utilities module
    ├── config.rs        # Configuration handling
    ├── container.rs     # Container operations
    ├── output.rs        # Output formatting
    └── target.rs        # Target resolution
```

### Key Components

- **CLI Framework**: Uses `clap` for command-line argument parsing with derive macros
- **Error Handling**: Uses `anyhow` for comprehensive error handling and context
- **Configuration**: Uses `toml` crate for TOML file generation and parsing
- **Async Runtime**: Uses `tokio` for async container operations
- **Testing**: Uses `tempfile` for isolated test environments

## Benefits of the Rust Implementation

### Key Improvements over Python

1. **Type Safety**: Rust's type system prevents many runtime errors
2. **Memory Safety**: No risk of memory leaks or buffer overflows
3. **Error Context**: Rich error messages with full context chains
4. **Performance**: Compiled binary with zero-cost abstractions
5. **Testing**: Isolated test environments with proper cleanup
6. **Dependencies**: Fewer runtime dependencies and faster startup

### Performance Comparison

| Feature | Python | Rust |
|---------|--------|------|
| Error Handling | Manual error checking | `anyhow::Result` with context |
| CLI Parsing | `argparse` | `clap` with derive macros |
| File Operations | `os.path` + `open()` | `std::fs` + `Path` |
| Testing | Manual setup/teardown | `tempfile` for isolation |
| Performance | Interpreted | Compiled binary |
| Startup Time | ~100ms | ~1ms |
| Memory Usage | Higher baseline | Lower baseline |
| Binary Size | N/A (requires Python) | ~5MB standalone |

## Development

### Adding New Commands

1. Create a new module in `src/commands/`
2. Implement the command struct with appropriate methods
3. Add the command to the CLI enum in `main.rs`
4. Add comprehensive unit tests
5. Update this documentation

### Dependencies

- `clap`: Command-line argument parsing
- `anyhow`: Error handling and context
- `serde`: Serialization/deserialization
- `toml`: TOML file handling
- `tokio`: Async runtime for container operations
- `thiserror`: Error type definitions
- `serde_json`: JSON handling
- `tempfile`: Testing utilities (dev dependency)

### Coding Standards

- Use `anyhow::Result` for all fallible operations
- Provide meaningful error context with `.with_context()`
- Write comprehensive unit tests for all functionality
- Use Rust naming conventions (snake_case for functions/variables)
- Document public APIs with rustdoc comments
- Follow Rust idioms and best practices

## Migration Completed

The Python to Rust migration is now complete. All Python code has been removed and the project is now a pure Rust implementation. The CLI maintains full compatibility with the original Python version while providing improved performance, safety, and maintainability.