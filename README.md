# Avocado CLI

Command line interface for Avocado.

- [Documentation](https://docs.peridio.com/avocado-cli)

## Install

### Homebrew (macOS and Linux)

```bash
brew tap avocado-linux/tap
brew install avocado-cli
```

Upgrade later with `brew upgrade avocado-cli`.

### Download a release

Prebuilt binaries for macOS (x86_64, arm64), Linux (x86_64 gnu/musl, aarch64 musl), and Windows are attached to each [GitHub Release](https://github.com/avocado-linux/avocado-cli/releases). Each tarball ships a single `avocado` binary; place it on your `PATH`.

Once installed, `avocado upgrade` performs an in-place self-update against GitHub Releases. If `avocado` was installed via Homebrew, the self-update still works, and it will remind you that `brew upgrade avocado-cli` keeps Homebrew in sync.
