# Changelog

All notable changes to `avocado-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **`--connect-sign` guidance.** The deploy help text and the Level 2 setup
  messages now reference `avocado connect trust promote-root --key <KEY>` with
  its required `--key` option, matching the CLI reference documentation.

## [1.0.0-rc.1]

Release candidate for 1.0.0.

**Stability commitment.** Starting with 1.0.0, the `avocado.yaml` configuration
schema and the generated runtime manifest are a stable contract. Within the
1.x series, changes to both are **additive only** — new optional keys and new
manifest fields may be introduced, but existing keys, their meaning, and the
shape of a produced manifest will not change in a backwards-incompatible way.
Breaking changes are reserved for the next major version. This release candidate
exists to exercise that contract in the field before it is frozen at 1.0.0.

The breaking cleanups that land the 1.0 baseline (see **Changed**/**Removed**)
are made now, in the RC, precisely so 1.0.0 can commit to the contract above.

### Added
- **Connect-signed TUF deploy.** A new `--connect-sign` flag on the deploy
  commands routes TUF metadata signing through Avocado Connect (`sign-for-deploy`
  API client + types, Connect-signed metadata path, `runtime_uuid` sent in the
  sign request). The prerequisite signing-key requirement is surfaced up front.
- **Per-target section overrides.** `rootfs`, `initramfs`, and `kernel` sections
  now honor `target-<name>:` override blocks, resolved on the composed config so
  path-based image sources are preserved.
- **Opt-in overlay preprocessing.** Overlay files can now be preprocessed at
  build time (opt-in).
- **`config show` signing state.** Per-runtime `signing_enabled` is now exposed
  in `avocado config show`.
- **VM software TPM.** VMs now provide a software TPM so tpm2-enabled images boot
  without stalling.

### Changed
- **Lockfile relocated to top-level `avocado.lock`.** The resolved lockfile now
  lives at the project root. Update any tooling or ignore rules that referenced
  the previous location.
- **Extension `type: path` mounts derived from config.** Path mounts are now
  computed directly from `avocado.yaml`; the separate `ext-paths.json` sidecar is
  gone (see **Removed**).
- **Release candidates ship as latest.** `-rc` tags now publish as full
  (non-prerelease) GitHub releases, so the CLI update check, `avocado upgrade`,
  and the Homebrew tap present release candidates to users as the latest version.
  `-alpha`/`-beta` tags remain internal-only prereleases. (#170)
- **VM guest networking.** VMs use `virtio-net-pci` so the guest NIC binds on the
  q35 machine type.

### Fixed
- **Standalone `avocado kernel image` on non-arm targets.** The command located
  the kernel by its arm64 `Image` name under `rootfs/boot/`, so it failed on
  x86-64 (where the kernel is `bzImage`). It now resolves the arch-normalized
  symlink `rootfs install` stages at `$AVOCADO_PREFIX/kernel/<kver>/Image` — the
  same path `runtime build` reads — so it works across architectures. (#171)
- **Deploy repo server startup race.** Fixed a race in the deploy repo server
  startup.
- **`cli_requirement` gating on pre-release builds.** A project's
  `cli_requirement` is now matched against the running version with any
  pre-release/build metadata stripped, so pre-release CLI builds (e.g. this
  release candidate) satisfy ordinary requirements like `>=0.25` or `^1` instead
  of being spuriously rejected by semver's pre-release matching rule.

### Removed
- **`ext-paths.json`.** Extension path mounts are now derived from config; the
  sidecar file is no longer written or read.

[1.0.0-rc.1]: https://github.com/avocado-linux/avocado-cli/releases/tag/1.0.0-rc.1
