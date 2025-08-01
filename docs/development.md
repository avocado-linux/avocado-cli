# Development

## Running Tests

This project is written in Rust. To run tests, you'll need to have Rust and Cargo installed.

```bash
cargo test
```

## Building the Project

To build the project in development mode:

```bash
cargo build
```

To build an optimized release version:

```bash
cargo build --release
```

## Running the CLI

During development, you can run the CLI directly with Cargo:

```bash
cargo run -- [command] [args]
```

For example:
```bash
cargo run -- init
cargo run -- sdk deps
```

## Code Formatting and Linting

Format your code with:
```bash
cargo fmt
```

Run the linter:
```bash
cargo clippy
```
