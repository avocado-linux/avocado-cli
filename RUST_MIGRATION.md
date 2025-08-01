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
```

### Testing

```bash
# Run all unit tests
cargo test

# Run only init command tests
cargo test commands::init::tests
```

## Architecture

### Project Structure

```
src/
├── main.rs              # CLI argument parsing and main entry point
└── commands/
    ├── mod.rs           # Commands module
    └── init.rs          # Init command implementation
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
- `sdk`: SDK management commands
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
- `tempfile`: Testing utilities (dev dependency)

### Coding Standards

- Use `anyhow::Result` for all fallible operations
- Provide meaningful error context with `.with_context()`
- Write comprehensive unit tests for all functionality
- Use Rust naming conventions (snake_case for functions/variables)
- Document public APIs with rustdoc comments