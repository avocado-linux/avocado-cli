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

- **`avocado cve report`.** Correlates the packages installed in a project with
  the CVEs of the recipes that produced them, reading the per-machine JSON a
  Yocto build publishes (`--file`) and every sysroot's RPM database. Results are
  broken down by scope — `sdk`, `rootfs`, `initramfs`, `target-sysroot`,
  `includes`, `includes:<name>` for legacy-layout remote extensions, which hold
  a database of their own, `runtime:<name>`, `ext:<runtime>/<name>`, and
  `ext:<name>` for extensions outside a runtime. `--fail-on-score <SCORE>` exits
  non-zero when any CVE reaches that CVSS value, for use as a release gate; the
  JSON document carries the resolved `score` and `score_source` the command
  itself ranks by, so a consumer gating on severity does not have to re-derive
  them, and `counts.cves_unscored` for the CVEs no threshold can match. Each
  affected package also carries `report_version_rpm`, the report's version put
  through the `-` → `+` rewrite RPM packaging applies, so a consumer can diff it
  against `installed_version` without reimplementing that rule.

  `--fail-on-score` is range-checked against the CVSS scale, so `99` — the
  0-100 confusion — is a startup error rather than a threshold that silently
  matches nothing. The JSON also carries `source.machine_mismatch`, the verdict
  behind the human warning, which `--output json` suppresses; and
  `packages_baseline_divergent`, for packages an extension or runtime holds at a
  version the rootfs holds differently, which its one-time seeded RPM database
  cannot distinguish from a package the scope installed itself.

  A sysroot whose `rpm -qa` fails is reported as a failed scan rather than as an
  empty one. `rpm -qa` over a wiped database exits 0 with no output instead, so
  emptiness is checked too: the command refuses to report when no sysroot
  yielded a package, and an existing-but-empty `rootfs` is kept as a scope —
  `scopes.rootfs.packages_scanned: 0` — rather than dropped, since dropping it
  left extension and runtime counts silently measured against no baseline. When
  either check fires, the tail of rpm's own stderr is quoted in the failure:
  the container exits 0 by design, so that diagnosis had nowhere else to go.
  `--runs-on` is refused rather than ignored, since the container helper this
  command uses reads the local volume.

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
