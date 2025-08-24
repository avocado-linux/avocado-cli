# Installation

The Avocado CLI is a Rust-based command line tool that can be installed using Cargo, Rust's package manager.

## Prerequisites

You'll need to have Rust and Cargo installed on your system. If you don't have Rust installed, you can get it from [rustup.rs](https://rustup.rs/).

## Install from GitHub Releases (Recommended)

The easiest way to install Avocado CLI is to download a pre-built binary from the GitHub releases page:

1. Visit [https://github.com/avocado-linux/avocado-cli/releases](https://github.com/avocado-linux/avocado-cli/releases)
2. Download the appropriate binary for your operating system
3. Extract the binary and place it in your PATH

## Install from Git

You can install the latest version directly from the Git repository using Cargo:

```bash
cargo install --git https://github.com/avocado-linux/avocado-cli.git
```

## Install from Source

Alternatively, you can clone the repository and build from source:

```bash
git clone https://github.com/avocado-linux/avocado-cli.git
cd avocado-cli
cargo install --path .
```

## Verify Installation

After installation, verify that the CLI is working correctly:

```bash
avocado --version
```

This should display the version information for the Avocado CLI.

## Updating

To update to the latest version, you can use the built-in upgrade command:

```bash
avocado upgrade
```

Alternatively, if you installed via Cargo, you can run the install command again:

```bash
cargo install --git https://github.com/avocado-linux/avocado-cli.git --force
```

The `--force` flag will overwrite the existing installation.