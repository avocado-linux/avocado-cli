# Installation

The Avocado CLI is a Rust-based command line tool that can be installed using Cargo, Rust's package manager.

## Prerequisites

You'll need to have Rust and Cargo installed on your system. If you don't have Rust installed, you can get it from [rustup.rs](https://rustup.rs/).

## Install from Git

You can install the latest version directly from the Git repository using Cargo:

```bash
cargo install --git https://github.com/avocado-framework/avocado-cli.git
```

## Install from Source

Alternatively, you can clone the repository and build from source:

```bash
git clone https://github.com/avocado-framework/avocado-cli.git
cd avocado-cli
cargo install --path .
```

## Verify Installation

After installation, verify that the CLI is working correctly:

```bash
avocado-cli --version
```

This should display the version information for the Avocado CLI.

## Updating

To update to the latest version, simply run the install command again:

```bash
cargo install --git https://github.com/avocado-framework/avocado-cli.git --force
```

The `--force` flag will overwrite the existing installation.