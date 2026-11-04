# Contributing

Thanks for helping improve the Avocado CLI.

## Questions, bugs, and ideas

- **Questions:** ask in the [Avocado Discord](https://www.peridio.com/join-our-discord).
- **Bugs and feature requests:** open an [issue](https://github.com/avocado-linux/avocado-cli/issues). For a bug, include the CLI version (`avocado --version`), your host OS, and the command that failed.
- **Security vulnerabilities:** do **not** open a public issue. See [SECURITY.md](./SECURITY.md).

## Code of conduct

This project follows the [Avocado Code of Conduct](https://docs.peridio.com/policies/coc). By taking part, you agree to follow it.

## Building and testing

You need a stable Rust toolchain with `rustfmt` and `clippy`, and `cmake`.

```bash
cargo build
```

Before you open a pull request, run the same checks CI runs on every pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --verbose
cargo test --verbose
```

CI also runs `cargo audit` and a Windows compile check:

```bash
cargo install cargo-audit --locked
cargo audit

# needs the MSVC toolchain, NASM, and cmake
cargo check --target x86_64-pc-windows-msvc --all-targets
```

## Pull requests

- Open pull requests against `main`. Keep each change focused, and describe what you changed and why.
- Add unit tests for each new feature or behavior change. For a bug fix, add a test that fails without the fix. Put unit tests in a `#[cfg(test)]` module next to the code, and integration tests in `tests/`.
- Make sure the checks above pass.

## Licensing of contributions

This project is provided under the [Apache License 2.0](https://github.com/avocado-linux/avocado-cli/blob/main/LICENSE).

By submitting a contribution (a pull request, patch, code, documentation, or any other material) for inclusion in this project, you agree that:

- **License.** Your contribution is licensed under the Apache License 2.0, the same terms as the rest of the project, as described in Section 5 of the license.
- **Right to contribute.** Your contribution is your original work, or you otherwise have the right to submit it, and submitting it does not violate any third party's rights or any agreement or law that applies to you.
- **No third-party personal data.** Your contribution does not contain personal information about any third party.
- **Nothing unlawful.** Your contribution is not defamatory, misleading, infringing, or otherwise unlawful.

Please do not submit anything you do not have the right to license this way.
