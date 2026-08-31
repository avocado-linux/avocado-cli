# Changelog

All notable changes to `avocado-cli` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`runtimes.<name>.var.recovery` and `avocado var-key` — an operator-held
  recovery key for the encrypted `/var`.** `var.recovery` names a registry
  secret (`avocado signing-keys create <name> --algorithm hmac-sha256`, a new
  secret key kind) that is a *master*; nothing derived from it enters a build.
  It is stored host-only, outside the signing-keys directory the SDK
  bind-mounts into build containers, so no build hook can read it; a master
  created before this lands is moved there on first use.
  `avocado var-key enroll <runtime> --device user@host` reads the device's SoC
  UID over SSH, derives that unit's passphrase as
  HMAC-SHA256(master, "avocado-var-recovery\0" || UID) and hands it to `avocadoctl var-key enroll`, which adds it as a LUKS2 keyslot
  (token `avocado-recovery`). `avocado var-key derive <runtime> --uid <UID>
  [--raw]` reproduces it on a bench to recover a unit whose hardware keyslot is
  gone. Once enrolled, the initrd retires the SoC-UID-derived keyslot.
- **`runtimes.<name>.var.hardware`** — `auto` (default: bind to whatever engine
  the machine ships, degrade and report), `caam` / `tpm2` (that engine must
  hold a keyslot; the initrd refuses to boot `/var` on the derived key when it
  is missing), `none` (no hardware keyslot; requires `var.recovery`). Validated
  at config load; an explicit choice rides in the initramfs as
  `/etc/avocado/var-hardware`.

### Fixed
- **`avocado install` / `rootfs install` / `initramfs install` exit non-zero
  when a sysroot install could not finish.** Three cases previously reported
  success and wrote a current install stamp: the sysroot could not be cleaned
  before a kernel change, the installed package versions could not be read
  back, or the kernel sysroot could not be staged. Each now fails with the
  reason (also emitted under `--json`) and leaves no stamp, so the next run
  repairs instead of reporting "up to date". The lock is still saved whenever
  packages landed, so a failed run cannot leave the kernel pin out of step
  with the disk.
- **`avocado install` follows the feed after `avocado update`.** `update` clears a
  target's version pins so the next install resolves the newest packages, but
  install only ran `dnf install`, which is additive and never moves packages that
  are already in the sysroot — so a rebuilt feed changed nothing until `clean`.
  When no pins are recorded (after `update`, or on a first install) the install
  now runs a `dnf distro-sync` on the sysroot in the same container pass; with
  pins present the lock stays authoritative and nothing is synced.
  `avocado update` also expires the SDK's dnf metadata cache (kept on the
  persistent SDK volume, default 48 h), so a feed whose contents changed under
  the same URL is re-read on the next install instead of days later.

## [1.0.0-rc.2] - 2026-08-28

### Added
- **`runtimes.<name>.signing.fit_key` — boot-FIT signing from the key registry.**
  Names an RSA PEM key (`avocado signing-keys import <name> --key --cert`, or
  `signing-keys create <name> --algorithm rsa2048`) that the runtime build
  materializes as `FIT.key`/`FIT.crt` for `mkimage`, so a signed boot image is
  reproducible from `avocado.yaml` alone. `signing.fit_unsigned: true` is the
  explicit opt-out. Replaces the interim `AVOCADO_FIT_KEY_DIR` /
  `AVOCADO_FIT_UNSIGNED` environment variables, which are no longer read.
  With `fit_key` set the build also re-packs the feed's bootloader so U-Boot
  enforces that key (`signing.fit_key_in_bootloader`, default true; i.MX8M via
  the feed's `imx-boot-tools/rekey-imx-boot.sh`), so provisioning writes a
  bootloader closed to the project key from the first flash.
- **`runtimes.<name>.var.encrypt: true` — encrypted `/var`.** Opts a runtime
  into a LUKS2 `/var` sealed to the target's hardware key store (OP-TEE fTPM
  on Jetson). The cli adds `cryptsetup-var` to the initramfs and
  `cryptsetup-var-udev` to the rootfs package sets and writes an
  `/etc/avocado/var-encrypt` marker into that runtime's initramfs; first boot
  encrypts the flashed var image in place, so seeded content survives. Unset
  is byte-identical to before. See `docs/features/encrypted-var.md`.
- **`image.verity: true` for extensions and the rootfs.** `avocado ext image`
  and the rootfs build run `veritysetup format` over the finished image and
  emit a hash tree (`<image>.verity`) plus its root hash. Extension root hashes
  land in the runtime manifest (`root_hash`, with the tree stored as
  `<image_id>.verity`); the rootfs root hash goes into the boot FIT as
  `avocado,roothash` and the tree into the machine's per-slot hash partition.
  Requires `veritysetup` in the SDK and `CONFIG_DM_VERITY` on the target.
  Provisioning carries everything; `avocado deploy` and `avocado connect
  upload` refuse a runtime with verity extensions until they publish the
  trees too.
- **The runtime build assembles the boot FIT** on machines whose feed ships a
  `fit-image.its` template, with this runtime's initramfs (previously a FIT
  machine booted the feed's initramfs), the rootfs root hash when set, and a
  signature with the key `runtimes.<name>.signing.fit_key` names.
- **Inter-extension dependencies via `depends_on`.** An extension can declare
  the extensions it builds on (`depends_on: [weston-base]`, optionally with a
  semver range); the CLI expands the closure everywhere it matters — fetch
  resolves and installs dependencies in one DNF transaction via virtual
  `avocado-ext(<name>)` capabilities, install seeds each dependent's rpmdb
  from its dependency (shared packages de-duplicated), the runtime list
  orders dependents ahead of their dependencies for merge priority, and
  install stamps carry a transitive fingerprint of the whole chain so a
  change anywhere underneath invalidates every dependent. Solver-derived
  dependencies are pinned in `avocado.lock` and replayed on clean checkouts;
  `ext fetch --locked` turns lock drift into an error for CI. An unresolved
  closure (missing dependency, cycle, version conflict) now fails `install`
  outright instead of silently installing a flat, unseeded list.
- **Device-tree overlays compile and deliver during builds.** An extension
  declaring `device_tree_overlays` gets the SDK tooling provisioned at
  `sdk install`, its `.dtbo`s compiled and delivered into the OS bundle at
  `avocado build` through per-BSP delivery hooks (verified on-device on
  Jetson Orin Nano and raspberrypi5).
- **Container Dev Mode.** Host CLI with an embedded registry and a VM push
  path for iterating on container workloads against the dev VM.
- **`--target-board` flag** for board interpolation across build commands.
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
- **`avocado vm update` migrates state instead of destroying it.** A version
  bump no longer resets the var disk: the host records the pending seed and
  attaches it read-only on the next start (found by btrfs UUID), and the
  guest adopts the release's runtime out of it before extensions merge —
  Docker volumes, image layers and installed SDKs survive the update.
- **`AVOCADO_OS_BUILD_ID` derives from the assembled work tree.** The id was
  an input list (package NEVRAs, then auth files) that missed overlay and
  `post_install` content; it is now `uuid5(ns, PKG_HASH:TREE_HASH)` where the
  tree hash covers exactly what `mkfs.erofs` preserves, taken before the
  identity append, on one shared rootfs/initramfs derivation. Any
  image-affecting change now OTAs. Every existing image re-ids once on the
  first rebuild after this release.
- **`permissions:` users and groups provision in stable sorted order.** They
  were HashMap-ordered, so auto-assigned UIDs/GIDs could differ between
  builds; the first build after this release may assign a different UID than
  the previous build did for users that omit `uid:`.
- **cpio archives are reproducible.** Entry order, inode numbering, and
  mtimes are normalized (`SOURCE_DATE_EPOCH`), matching the erofs side.
- **Rootfs and initramfs images no longer ship package-manager state.** The
  rpmdb, `var/lib/dnf` and `var/cache/dnf` are removed from the staged copy
  before the image is built — measured at ~13MB of a qemux86-64 rootfs and
  14MB of a 123MB initramfs. Nothing on target reads any of it; these systems
  have no runtime package manager. Only the staged copy is touched, so the
  shared sysroot keeps its database and `ext install` / `runtime install`
  still seed their installroots from it. (dnf's own logs never land in the
  staged copy at all — dnf does not prefix `logdir` with the installroot —
  so they need no removal; they live under the SDK prefix.)

  Anything inspecting a built image for installed packages — `rpm -qa` against
  a loop-mounted rootfs, say — needs to query the sysroot or the lockfile
  instead. This is a necessary step toward reproducible images but not a
  sufficient one on its own; archive mtime normalization is separate.
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
- **`avocado build` refuses a stale rootfs or initramfs install stamp.** An
  `overlay/` edit followed by `avocado build` alone used to build from the
  previous overlay content with exit 0 and an unchanged `AVOCADO_OS_BUILD_ID`,
  so the update pipeline never offered it. The build now validates the install
  stamps against the current inputs, the same check `install` already made.
- **The build id covers everything the image ships.** The tree hash no longer
  prunes `var/cache` and `var/log` wholesale (only `var/cache/dnf` is actually
  purged from the image), and the initramfs hash includes file ownership, which
  `cpio` preserves. A change under either previously reached the image without
  moving `AVOCADO_OS_BUILD_ID`, so devices running different bytes were never
  offered the update. Images re-id once.
- **`ext fetch --locked` fails on an extension the lock has no entry for**, and
  never writes the lock. It previously gated only edited pins and moved
  implied dependencies, letting a newly declared extension resolve against
  feed head and be recorded silently.
- **Signature files carry the whole signature.** `create_signature_content`
  assumed 64 bytes; an RSA signature was truncated to 64 bytes in `.sig` files
  by `sign_hash_manifest` and made `sign_file` panic. The length is now the
  algorithm's.
- **Editing an `overlay/` file now invalidates the install stamp.** The stamp
  hashed only the overlay's config value (dir name), so `avocado build` after
  an overlay edit shipped the previous file with no warning; the content
  digest now always folds in (verbatim and preprocessed overlays alike, and
  bare-string `overlay: mydir` declarations hash the right tree).
- **`ext build` resolves `$CC` from the cross-canadian bindir.** The
  target-sysroot gcc it found before is a target ELF that only fails on a
  cross-arch host, which same-arch CI never caught; the SDK env vars are now
  required up front and CI gained a qemuarm64 leg.
- **Extension packaging emits RPM-safe versions for pre-releases** and works
  against an rpm 4.20 SDK sysroot; `target-<name>:` overrides are honored on
  every ext path, recursively; extension versions resolve from the
  extension's own source tree.
- **Registry-name signing keys resolve in provision, sign, and `sdk run`;**
  deploy surfaces container script stderr instead of a generic failure;
  `connect auth` honors `--org` before the zero-orgs shortcut; PTY allocation
  derives from the environment; abandoned build volumes are reaped.
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
  regardless of `runtimes.<name>.target`, which in a multi-target config
  provisioned target-dev for runtimes the user had not asked to build. The scan
  reads the merged runtime config, so a `compile:` reference declared inside a
  `target-<t>:` override block counts too — selecting runtimes from resolved
  config while scanning unresolved config meant no target-dev sysroot was
  installed for such a section, yet `runtime build` still ran its compile
  script.
- **A runtime that names its `target:` only inside a `target-<t>:` block is no
  longer treated as targeting everything.** Override resolution strips the
  non-matching blocks before target selection reads the runtime, so such a
  runtime arrived with no `target:` key at all and matched the
  "no target declared, applies to every target" branch. `avocado sdk install
  --target qemux86-64` would install a raspberrypi4-only runtime's compile
  packages into the x86-64 target sysroot. A `target-<t>:` block for the target
  being built still keeps a runtime in scope even when it declares no `target:`
  of its own. This also scopes the extension and runtime steps of `avocado
  install`, which shared the selection logic through a duplicate that has been
  collapsed onto the fixed one.
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
