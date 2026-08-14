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
- **`avocado sbom`.** Emits an SPDX 3.0.1 JSON-LD document describing the
  packages installed in a project — `software_Sbom` with
  `software_sbomType: deployed`, one root per sysroot scope.

  The SDK and target sysroot are excluded: they run on the build host and ship
  nothing, unless `--include-sdk` is passed.

  Each package carries its purl, license expression, supplier, homepage, build
  time, and the sha256 of its rpm header, plus the producing recipe read from
  `SOURCERPM`, so a device SBOM names the recipe behind a package without being
  joined against the build's pkgdata. Yocto license strings are translated to
  SPDX expressions (`&`/`|` to `AND`/`OR`).

  A package's epoch is part of its identity, so it is queried and travels in
  both the version (`1:3.5.6-r0.2`) and the purl's `epoch` qualifier: without
  it `1:2.39-r0.2` and `2.39-r0.2` (different packages by rpm's own ordering)
  would collapse into one element.

  An extension's installroot is seeded with a copy of the rootfs RPM database,
  and those packages are excluded from what the extension reports. The seed is
  identified by RPM install *transaction*: every package of one `dnf install`
  shares an `INSTALLTID`, the seed arrives as whole transactions, and a
  transaction is the seed exactly when every package in it is one the rootfs
  also carries by name. So an extension that installs a package the rootfs
  also has keeps it (its transaction added something new), a rootfs that has
  since upgraded a package does not turn the stale seeded copy — or the rest
  of its transaction — into extension content, and `install --force`, which
  reinstalls the whole rootfs in one new transaction, no longer detaches the
  seed from anything recognisable. The remaining ambiguity is an extension
  transaction holding only packages the rootfs already carries by name: it
  reads as seed, so the extension loses its containment for them, though they
  remain in the document under the rootfs. A seeded scope that reports at
  least as many packages as the rootfs is called out on stderr, since that is
  the shape this subtraction failing takes.

  An extension is described under the runtime that carries it rather than
  beside it: the runtime `contains` its extensions and is the only one of them
  that is a root, so the composition declared in `avocado.yaml` survives into
  the document. A legacy `ext:<name>` scope names no runtime, and an extension
  whose runtime installed nothing of its own has no element to hang from —
  both stay roots rather than being guessed at.

  The document carries the feed it came from: the release, channel, and
  immutable snapshot the lockfile pinned, with an `externalRef` locating that
  snapshot's subtree. Without it the document is unjoinable — nothing would say
  which feed produced these RPMs, so it could not be tied back to the build's
  own SPDX documents or re-resolved later. Only the lockfile's pin is used;
  the config's declared release and channel say what was asked for rather than
  what was resolved, and a channel head moves. An absent or unreadable lockfile
  costs that pointer and nothing else — the inventory itself comes from the RPM
  databases.

  Element ids are derived from the installed set, so the same set produces the
  same document twice running rather than churning under a consumer diffing two
  of them. `created` is the one field a clock would move, so `SOURCE_DATE_EPOCH`
  pins it when set — with it the document is byte-stable and can be referenced
  by content hash; unset or unparseable, it reports the current time, since a
  typo silently dating every document to 1970 is worse than one that is honestly
  not reproducible.

  The command never transmits the document. It writes the file and, with `-o`,
  points at <https://tools.spdx.org/app/validate/> for anyone who wants the
  reference SPDX tools' verdict, along with the reason to think first: that
  service stores every upload and serves it back without authentication for
  about ten days, and an SBOM is a component inventory of a shipped product.
  Whether a given document can be published is the operator's call, on a
  document they can read first.

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
- **VM routing notices moved from stdout to stderr, and go quiet under
  `--output json`.** The Docker-Desktop routing step runs before the command
  dispatches, so its warnings - VM not running, `AVOCADO_VM_DIR` unset, docker
  socket forward missing, stale manifest, plus the non-fatal hiccups from an
  auto-start - used to land on the stdout of whatever command followed.
  `avocado sbom > sbom.json` writes an SPDX document there, so a `[WARNING]`
  ahead of it left a file no consumer could parse. These describe the
  environment rather than the result, so they are now on stderr.

  Under `--output json` they are dropped entirely, matching the upgrade banner:
  the avocado-desktop CLI runner merges stdout and stderr in causal order and
  parses the result, so stderr alone does not make a line safe. **The cost is
  real**: `avocado vm update --output json` no longer reports a failed
  hibernation-supervisor bind at all, and exits 0. Anything that needs those
  diagnostics should run without `--output json`.
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
- **`sdk run` passes its argv through literally; the container shell no longer
  re-splits or expands it.** Anything passing a `$VAR`, a glob, or a `&&` chain
  as a single argument and counting on the container shell to interpret it now
  gets that string through as one command word: `avocado sdk run -- 'ls /foo &&
  ls /bar'` no longer runs two commands, and `avocado sdk run -- echo '$HOME'`
  prints a literal `$HOME`. Ask for a shell explicitly instead — `avocado sdk
  run -- bash -lc 'ls /foo && ls /bar'` — which is the form the flags already
  implied and which only works correctly after this change.

  This one breaks silently at runtime: there is no error and no warning, so a
  pinned-version bump in CI will not fail the build, it will just run a
  different command. Audit any job that shells through `sdk run` before
  upgrading.

  **What it fixes.** The arguments after `--` were spliced into the container's
  script with a bare `join(" ")`, so the shell inside re-parsed them. `avocado
  sdk run -- bash -lc 'U=/opt/x; ls $U'` arrived as `bash -lc U=/opt/x; ls $U`,
  which the shell read as two commands and whose `$U` the *outer* shell expanded
  to nothing — printing plausible output for a different directory rather than
  failing. Each element is quoted now, so argv is argv, matching `docker run`
  and `kubectl exec`.
- **`is_version_compatible` returns `Option<bool>` instead of `bool`.** Only
  affects consumers of the `avocado_cli` lib target. `None` means the remote
  version could not be read at all, which the old `bool` could not express -
  it collapsed "definitely older" and "unreadable" into the same `false`, and
  that collapse is what let an unparseable version fall through as a pass.

### Fixed
- **`source_date_epoch` is honored outside extension images.** The key was
  read only by `ext image`, which exports it inside its own script. The rootfs
  build script has always passed `-T "${SOURCE_DATE_EPOCH:-0}"` to
  `mkfs.erofs`, but nothing ever set the variable, so a project that
  configured an epoch got a reproducibility stamp on its `.raw` extensions and
  a silently ignored key everywhere else — including in `post_build` hooks,
  which run in their own container. Rootfs, initramfs and `post_build` runs
  now all carry it.

  Note the key still behaves two ways by design: absent config leaves the
  variable unset for rootfs/initramfs/`post_build`, because
  `SOURCE_DATE_EPOCH` changes the behavior of tools well beyond ours (gzip,
  tar, python bytecode) and hooks should not inherit that unasked. `ext image`
  continues to export `0`.
- **A runtime package's `compile:` section now counts as active.** Only
  `kernel.compile` and extension packages were scanned, so a section reached
  through `runtimes.<name>.packages.<pkg>.compile` was treated as unused: no
  target-dev sysroot was provisioned and the section's own `packages:` were
  dropped without a word. The build then failed much later on missing target
  headers or libraries, with nothing pointing back at the cause.

  That scan is also scoped to the current target now, the way the sibling
  extension scan already was. It previously read every runtime in the file
  regardless of `runtimes.<name>.target` or `--runtime`, which in a
  multi-target config provisioned target-dev for runtimes the user had not
  asked to build.
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
