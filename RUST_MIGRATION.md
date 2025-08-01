# Rust Migration Guide

This document describes the migration of the Avocado CLI from Python to Rust.

## Overview

The Avocado CLI is being migrated from Python to Rust to improve performance, reduce dependencies, and provide better error handling. This migration maintains full compatibility with the existing command-line interface while providing a more robust implementation.

## Current Status

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
```

### Testing

```bash
# Run all unit tests
cargo test

# Run only init command tests
cargo test commands::init::tests

# Run only SDK command tests
cargo test commands::sdk::tests
```

## Architecture

### Project Structure

```
src/
├── main.rs              # CLI argument parsing and main entry point
├── commands/
│   ├── mod.rs           # Commands module
│   ├── init.rs          # Init command implementation
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
- **Configuration**: Uses `toml` crate for TOML file generation
- **Testing**: Uses `tempfile` for isolated test environments

## Migration from Python

### Comparison with Python Implementation

| Feature | Python | Rust |
|---------|--------|------|
| Error Handling | Manual error checking | `anyhow::Result` with context |
| CLI Parsing | `argparse` | `clap` with derive macros |
| File Operations | `os.path` + `open()` | `std::fs` + `Path` |
| Testing | Manual setup/teardown | `tempfile` for isolation |
| Performance | Interpreted | Compiled binary |

### Key Improvements

1. **Type Safety**: Rust's type system prevents many runtime errors
2. **Memory Safety**: No risk of memory leaks or buffer overflows
3. **Error Context**: Rich error messages with full context chains
4. **Performance**: Compiled binary with zero-cost abstractions
5. **Testing**: Isolated test environments with proper cleanup

## Future Work

### Planned Commands

- `clean`: Clean build artifacts
- `build`: Build runtime images  
- `runtime`: Runtime management commands
- `ext`: Extension management commands

### Migration Strategy

1. Implement commands one by one, maintaining Python compatibility
2. Add comprehensive unit tests for each command
3. Gradually replace Python implementation
4. Maintain integration tests to ensure compatibility

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
- `tempfile`: Testing utilities (dev dependency)

### Coding Standards

- Use `anyhow::Result` for all fallible operations
- Provide meaningful error context with `.with_context()`
- Write comprehensive unit tests for all functionality
- Use Rust naming conventions (snake_case for functions/variables)
- Document public APIs with rustdoc comments