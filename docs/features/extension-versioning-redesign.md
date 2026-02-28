# Extension Versioning Redesign: Config as Source of Truth

## Context

Avocado Linux is migrating to Tekton CI with year-based feed structure (`2024/edge/`, `2024/stable/`). Extension version fields currently use `{{ avocado.distro.version }}` interpolation, which creates cascading problems: wildcards that can't resolve for git/path extensions, mismatches between config and RPM versions, and overloaded semantics where `distro.version` is simultaneously a repo path component, a package version spec, and an extension version.

**Solution:** Separate concerns. Config is the single source of truth for extension versions (explicit semver, maintained in PRs). `distro.version` → `distro.release` (feed year, not a package version). Core packages use `"*"` — repo scoping via `--releasever` handles feed selection, lock file handles pinning. Repo config migrates from `sdk.*` to `distro.repo.*`.

## Design

### Extension versions: config is source of truth

The `version` field in an extension's `avocado.yaml` IS the version. Always. For all source types. No interpolation, no RPM DB queries. CI builds whatever version the config says. Versions bumped explicitly in PRs.

Keep semver — unified versioning across all output artifacts. Distro extensions use `YYYY.S.PATCH` convention (e.g., `2024.0.0`).

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

### Remaining (separate repos/PRs)

8. **Tekton CI (iac repo)**: Remove `DISTRO_VERSION` param/env from build-extensions-machine.yaml; future rename of `distro-codename` param
9. **avocado-os configs**: Remove `{{ avocado.distro.version }}` from extension versions, set concrete semver, change package specs to `"*"`, rename `distro.version` → `distro.release`
10. **Internal rename**: Update ~25 call sites from `get_sdk_repo_url()`/`get_sdk_repo_release()` to `get_repo_url()`/`get_releasever()`. Rename `RunConfig.repo_release` → `releasever` and update container shell scripts. (Deprecated wrappers work in the meantime.)
