# Extension Versioning Redesign: Config as Source of Truth

## Context

Avocado Linux is migrating to Tekton CI with year-based feed structure (`2024/edge/`, `2024/stable/`). Extension version fields currently use `{{ avocado.distro.version }}` interpolation, which creates cascading problems: wildcards that can't resolve for git/path extensions, mismatches between config and RPM versions, and overloaded semantics where `distro.version` is simultaneously a repo path component, a package version spec, and an extension version.

**Solution:** Separate concerns. Config is the single source of truth for extension versions (explicit semver, maintained in PRs). `distro.version` → `distro.release` (feed year, not a package version). Core packages use `"*"` — repo scoping via `--releasever` handles feed selection, lock file handles pinning. Repo config migrates from `sdk.*` to `distro.repo.*`.

## Design

### Extension versions: config is source of truth

The `version` field in an extension's `avocado.yaml` IS the version. Always. For all source types. No interpolation, no RPM DB queries. CI builds whatever version the config says. Versions bumped explicitly in PRs.

Keep semver — unified versioning across all output artifacts. Distro extensions use `YYYY.S.PATCH` convention (e.g., `2024.0.0`).

#### One deliberate exception: the RPM `Version:` field

RPM's grammar forbids `-`, so a semver pre-release cannot be an RPM `Version:` verbatim — `rpmbuild` rejects `1.0.0-rc.1` with `Illegal char '-'`. Pre-release versions are therefore stored in the RPM as `1.0.0~rc.1` (and build metadata as `^`), which is the form that also gives RPM the right ordering: `1.0.0~rc.1` sorts before `1.0.0`, matching semver precedence.

This is the one place a version is *not* the config's string, so it is confined as tightly as possible:

- Only the spec's `Version:` field and the NVR filename use the RPM form. `utils::version::to_rpm_version` is the single place it is produced.
- Everything else stays semver — config, the `avocado.yaml` baked into the package payload, the version the platform records for a published extension.
- The mapping is injective, so `from_rpm_version` inverts it exactly. Any version read back *out* of rpm (an rpmdb query, a parsed NVR) must go through it, or it will not match the config it came from.
- A `-` *inside* a pre-release identifier (`1.0.0-rc-1`) is rejected rather than mapped: `~` is a precedence operator to RPM, so mapping it would invert ordering and dnf would refuse the upgrade. Use `.` to separate identifiers.

So config remains the source of truth; the RPM form is a representation of it at one boundary, not a second version.
### Version providers: reading the version from the source tree

Some extensions wrap a program maintained in the same repo (avocado-cli, avocado-conn, avocado-rat) and want the extension version to track that program's version without a second place to bump. `version` therefore also accepts a mapping that names a file inside the **extension's own source tree**:

```yaml
extensions:
  avocado-ext-cli:
    version:
      file: Cargo.toml        # relative to the extension root; no `..`, no absolute paths
      key: package.version    # dot path; format inferred from the extension

  some-other-ext:
    version:
      file: VERSION           # no `key` => whole file, trimmed
```

`key` is the discriminator. Absent means read the file and trim it, with no parsing and no format guessing — so `VERSION`, `version.txt`, and `.version` all work, and `file: Cargo.toml` with no `key` is a literal read (which then fails semver, as it should). Present means parse the file and navigate the dot path; `format` (`toml`/`json`/`yaml`) is inferred from the extension and can be set explicitly. Only the extension's direct-child `version:` supports the mapping form — a `target-*:`/`kernel-*:` override must use a literal.

**Why a file and not `{{ env.VAR }}`.** These extensions previously declared `version: '{{ env.AVOCADO_EXT_VERSION }}'`, with CI reading Cargo.toml and exporting the variable. `version` is an *identity* field, but `env` binds it to the **caller's** environment — and the packaging job and a downstream consumer are by definition different environments. Consuming such an extension via `source: { type: git | path }` interpolated the version to `""` (a warning, not an error) and then failed semver validation, which is exactly the failure class listed in the Context above. A file inside the extension's own tree is present in every consumption mode — the working copy for `path`, the clone for `git`, the RPM payload for `package` — so the version is a pure function of the extension.

`{{ avocado.* }}` and `{{ config.* }}` remain fine elsewhere in an extension config: those are consumer-context values that are *supposed* to differ per build. It is specifically `env` in an identity field that cannot work.

**No baking, for providers.** The published `avocado.yaml` keeps the provider rather than a resolved literal: the RPM payload *is* the source tree, so the config resolves itself. `ext package` guarantees this by always adding the provider's file to the package payload, including when the extension declares an explicit `package_files` list.

`config_edit::bake_extension_version` still exists for the legacy `{{ env.AVOCADO_EXT_VERSION }}` form, which genuinely cannot resolve downstream and must be baked. `ext package` skips the bake entirely for a provider-based extension — baking one would strand the provider's `file:`/`key:` lines under a replaced `version:` scalar (see `test_bake_extension_version_would_corrupt_a_provider_block`). The bake goes away once no extension uses the env form.

### Rollout

The provider cannot be adopted by an extension until a CLI that understands it has been *released*, because `setup-avocado-cli` installs `latest` and the release workflow for a program-tracking extension runs on the same tag that publishes the new CLI. Hence two steps:

1. **This change** — the provider, the reader extraction, `config show --detail` reporting `version`. No extension migrates yet; `avocado-cli`, `avocado-conn`, and `avocado-rat` stay on `{{ env.AVOCADO_EXT_VERSION }}` and keep working through the bake.
2. **After a CLI release carrying step 1** — migrate those three `avocado.yaml` files to `version: { file: Cargo.toml, key: package.version }`, drop `AVOCADO_EXT_VERSION` and the `ext-version` input from their workflows and from `avocado-linux/actions`, and delete the bake.

**Compatibility, at step 2.** Publishing a provider-based extension is a payload format change. A CLI predating providers reads the mapping, coerces it to `"0"`, and fails with `invalid version '0'`. There is no graceful degradation: `cli_requirement` is a top-level field and is not merged from a remote extension's config, so an extension cannot declare a CLI floor a consumer will honor. Publish migrated extensions to `next` first and verify before promoting.

### Rename `distro.version` → `distro.release`

`distro.version` was overloaded — it was used as a repo path component, a package version spec, an extension version source, and passed as runtime env vars. It should only be the **release family identifier** (feed year).

`distro.release` has exactly two functional roles:

1. **Repo path construction**: Combined with `distro.channel` → `2024/edge` → used as DNF `--releasever`
2. **Lock file compatibility guard**: Detects incompatible feed year changes (2024 → 2026)

It is NOT used as a package version spec or extension version.

### Naming convention: `releasever` for the composed value

The composed value `{distro.release}/{distro.channel}` (e.g., `2024/edge`) is called **`releasever`** internally — matching DNF terminology. This replaces the old `repo_release` naming. `codename` is reserved for a future avocado build schema version.

- Rust method: `get_releasever()` (replaces `get_sdk_repo_release()`)
- Internal variable: `releasever` (replaces `repo_release`)
- `RunConfig` field: `releasever` (replaces `repo_release`)
- Container shell var: `RELEASEVER` (replaces `REPO_RELEASE`)

### Environment variables

New primary env vars with legacy fallbacks:

| Config field                       | New env var (primary)    | Legacy env var (fallback)  |
| ---------------------------------- | ------------------------ | -------------------------- |
| `distro.repo.url`                  | `AVOCADO_REPO_URL`       | `AVOCADO_SDK_REPO_URL`     |
| `distro.repo.releasever` / derived | `AVOCADO_RELEASEVER`     | `AVOCADO_SDK_REPO_RELEASE` |
| `distro.release`                   | `AVOCADO_DISTRO_RELEASE` | —                          |
| `distro.channel`                   | `AVOCADO_DISTRO_CHANNEL` | —                          |

Priority chains:

- **repo URL**: `AVOCADO_REPO_URL` > `AVOCADO_SDK_REPO_URL` > `distro.repo.url` > `sdk.repo_url` (legacy) > None
- **releasever**: `AVOCADO_RELEASEVER` > `AVOCADO_SDK_REPO_RELEASE` > `distro.repo.releasever` > `sdk.repo_release` (legacy) > derived `{release}/{channel}`
- **distro.release**: `AVOCADO_DISTRO_RELEASE` > config `distro.release` (aliased from `distro.version`)
- **distro.channel**: `AVOCADO_DISTRO_CHANNEL` > config `distro.channel`

### Migrate repo config from `sdk.*` to `distro.repo.*`

`sdk.repo_url` and `sdk.repo_release` lived under the SDK section but affected ALL commands (ext, runtime, sdk). They now live under `distro.repo.*`.

**Single repo now, multi-repo later.** `distro.repo` (singular) for the primary avocado repo. Future PR adds `distro.repos` (plural map) for private repos alongside the avocado open source repo.

```yaml
distro:
  release: 2024        # feed year — Yocto LTS stream
  channel: edge        # stability channel
  # releasever derived as "2024/edge" for DNF --releasever

  repo:                # primary repo config (migrated from sdk.repo_*)
    url: "https://repo.avocadolinux.org"    # default
    # releasever: "2024/edge"               # explicit override (rarely needed)

# Future: distro.repos (plural map) for multi-repo support
# distro:
#   repos:
#     avocado:
#       url: "https://repo.avocadolinux.org"
#     my-company:
#       url: "https://rpm.mycompany.com"
#       gpgkey: "https://rpm.mycompany.com/RPM-GPG-KEY"
```

### Core package specs use `"*"`

`sdk/install.rs` used `get_distro_version()` as the version spec for core packages. With `distro.release: 2024` (just the year), this would produce `avocado-pkg-rootfs-2024` which won't match RPM versions like `2024.0`.

Changed to `"*"` — the repo is already scoped via `--releasever`, lock file pins exact versions.

### Target config

```yaml
distro:
  release: 2024
  channel: edge
  repo:
    url: "https://repo.avocadolinux.org"

runtimes:
  dev:
    packages:
      avocado-runtime: "*"        # repo scoping + lock file

extensions:
  app:
    version: "2024.0.0"          # explicit semver, maintained in PRs

sdk:
  image: "docker.io/avocadolinux/sdk:{{ config.distro.channel }}"
  packages:
    avocado-sdk-toolchain: "*"   # repo scoping + lock file
```

## Implementation Status

### Completed (avocado-cli)

1. **Consolidated `validate_semver()`** into shared `src/utils/version.rs` module, replacing three private copies in package.rs, build.rs, image.rs
2. **Removed wildcard version resolution** from `ext build` and `ext image` — config is source of truth, no RPM DB fallback. Removed `query_extension_rpm_version()` from both files.
3. **Renamed `distro.version` → `distro.release`** in `DistroConfig` struct (with `version` as serde alias), updated `AvocadoContext` in interpolation system, added `avocado.distro.release` path with `avocado.distro.version` alias
4. **Migrated repo config** from `sdk.*` to `distro.repo.*` — added `DistroRepoConfig` struct, new `get_repo_url()` and `get_releasever()` methods with full env var priority chains, kept deprecated `get_sdk_repo_url()`/`get_sdk_repo_release()` wrappers
5. **Core packages use `"*"`** in `sdk/install.rs` — all four `get_distro_version()` calls replaced with `"*"`
6. **Added `distro_release` to lock file** — `LockFile` struct, `check_distro_release_compat()` method, populated in sdk/ext/runtime install commands
7. **Updated default config template** — `distro.release`, `"*"` for runtime and SDK packages
8. **Version providers** — `version: { file, key }` in `src/utils/ext_version_source.rs`, resolved during composition (`load_composed_with_board`) and in `get_merged_section_with_board` (the path `ext package` takes for a local extension). Extracted the four-strategy extension-tree read ladder out of `config.rs` into `src/utils/ext_source_reader.rs` so the provider reads the version file by the same route the config was read. `get_package_files` always ships the provider's file; `ext package` skips the bake for provider-based extensions. `config show --detail` reports each extension's resolved `version`.

### Remaining (separate repos/PRs)

0. **Adopt version providers** (after a CLI release carrying item 8): migrate `avocado-cli`, `avocado-conn`, `avocado-rat` to `version: { file: Cargo.toml, key: package.version }`; drop `AVOCADO_EXT_VERSION` / the `ext-version` input from their workflows and from `avocado-linux/actions`; have the reusable release workflow read the version via `avocado config show --detail --output json`; delete `bake_extension_version`. See Rollout above.

8. **Tekton CI (iac repo)**: Remove `DISTRO_VERSION` param/env from build-extensions-machine.yaml; future rename of `distro-codename` param
9. **avocado-os configs**: Remove `{{ avocado.distro.version }}` from extension versions, set concrete semver, change package specs to `"*"`, rename `distro.version` → `distro.release`
10. **Internal rename**: Update ~25 call sites from `get_sdk_repo_url()`/`get_sdk_repo_release()` to `get_repo_url()`/`get_releasever()`. Rename `RunConfig.repo_release` → `releasever` and update container shell scripts. (Deprecated wrappers work in the meantime.)
