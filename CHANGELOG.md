# Changelog

All notable changes to `avocado-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Build commit in `avocado --version`.** The version line now reports the
  commit the binary was built from and its date, following rustc's shape:
  `avocado 1.0.0-rc.1 (abc1234 2026-03-05)`. A bug report now identifies the
  exact build. Builds made outside a git checkout - or inside an unrelated
  one, such as a vendored copy in another repository - keep reporting the bare
  version rather than embedding a commit that is not this binary's. Anything
  parsing this line must read the **second** whitespace-separated field, since
  the last field is now a date.

  **Rollout note.** That instruction only reaches parsers written from here
  on. Every already-released `avocado` reads the remote's version with
  `.last()` over the whole `--version` output, so during the window where an
  older local CLI talks to a newer remote it takes `2026-03-05)` as the remote
  version, fails to parse it, and - on those releases - falls open rather than
  refusing: the minimum-remote-version check is skipped and a date is printed
  as the version. Nothing can be shipped that fixes those binaries
  retroactively, so upgrade the local side first. Moving the build detail to a
  second output line would not have avoided this: the shipped parser splits the
  entire stdout blob, not its first line, so `.last()` still reaches it.
  `avocado-desktop` is not affected - its parser already reads the first line's
  second field.

### Changed
- **Extension images no longer ship package-manager state.** `var/lib/rpm`,
  `var/lib/dnf` and `var/cache/dnf` are excluded from the built `.raw` —
  measured at 13.4M of a 22M `avocado-ext-tunnels` sysroot, against an 8.2M
  `/usr` payload. Nothing on target can read it: sysext and confext merge
  `/usr`, `/opt` and `/etc`, never `/var`.

  Excluded at image time rather than deleted, because `ext image` runs mkfs
  against the live sysroot with no work copy, and later `ext dnf` / `ext
  install` calls still resolve against that database. Configured `var_files`
  patterns are unaffected.
- **A skipped remote version check no longer reports as a passed one.** When
  the remote's `--version` output cannot be parsed, `--runs-on` used to print
  `[SUCCESS] Remote avocado version: <whatever it read>`, which is a green line
  for a comparison that never happened. It now prints a `[WARNING]` saying the
  check was skipped, on a path an active renderer or `--json` cannot swallow,
  and emits a `{"event":"warning"}` line on the NDJSON stream so a consumer
  reading only that stream does not render an unqualified green run.
  `--runs-on localhost` reports neither: it is the same machine, so the check
  is skipped with nothing to say rather than asserting a version it never read.

### Breaking
- **`is_version_compatible` returns `Option<bool>` instead of `bool`.** Only
  affects consumers of the `avocado_cli` lib target. `None` means the remote
  version could not be read at all, which the old `bool` could not express -
  it collapsed "definitely older" and "unreadable" into the same `false`, and
  that collapse is what let an unparseable version fall through as a pass.

### Fixed
- **Rootfs and initramfs no longer reinstall on every run.** `avocado sdk
  install` wiped and rebuilt both sysroots from scratch on every invocation,
  even with nothing changed. Removal detection compared the lockfile against
  *config-declared* packages only, so the per-kernel packages the install
  auto-appends (`packagegroup-avocado-{rootfs,initramfs}-modules-<kver>` and
  `kernel-image-<kver>`) read as "removed" from the second run onward and
  forced a clean reinstall. It now compares against the effective set —
  config packages plus those auto-appends.
  As a second-order effect of that false positive, the same path dropped those
  packages' version pins from the in-memory lockfile, so dnf resolved the
  kernel image and module packagegroup to newest-available instead of the
  locked NVR. `avocado.lock` looked pinned on disk but the pin never applied;
  it now binds.
- **Unchanged sysroots are skipped outright.** The rootfs and initramfs install
  stamps were written but never read, so each run still paid a kernel
  repoquery, a dnf transaction, and a lockfile rewrite. Both stamps are now
  read (batched into one container invocation) and a current stamp short-
  circuits the install. The stamp hash covers the effective package set,
  `sdk.repo_url`, `sdk.repo_release`, `sdk.disable_weak_dependencies`, and a
  digest of the sysroot's lockfile pins, so a snapshot bump, a feed switch, or
  an `avocado unlock` all invalidate it. `avocado {rootfs,initramfs} clean` now
  removes the install stamp along with the sysroot, and `--no-stamps` still
  forces a full reinstall.
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
- **Abandoned build volumes reclaimed automatically.** `install` and `build` now
  delete provably-abandoned per-project `avo-*` Docker volumes (source directory
  gone, or `.avocado-state` missing or pointing at a different volume) when a new
  volume is created, so they no longer accumulate after a project directory is
  removed without `avocado clean`. A stale or half-populated volume now fails the
  rootfs build with an actionable message pointing at `avocado clean`/`avocado
  prune` instead of a cryptic `grep: .../etc/passwd` error. (#178)

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
