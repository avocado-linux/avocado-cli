# Docker Image Priming & Var Partition Provisioning

## Context

Avocado runtimes build a var partition (`avocado-image-var-*.btrfs`) containing extension `.raw` images and avocado metadata. However, there's no way to ship pre-pulled Docker images or inject arbitrary files into the var partition. Devices that need Docker containers available at first boot must pull images over the network, which is slow and may not be feasible in air-gapped environments.

We need to support priming Docker's image cache on the var partition at build time so Docker ships with images locally pulled and available, and more generally support adding files to `/var` from extensions.

**Prior art:** Yocto's `meta-virtualization` layer (`container-cross-install.bbclass`, `vrunner.sh`) solves this by booting a QEMU VM with dockerd, importing OCI images via skopeo, and exporting `/var/lib/docker/` as a tarball. We adapt a similar approach using Docker-in-Docker inside the SDK container.

## Config Schema

### First-class Docker image priming (extension-level)

Extensions declare `docker_images` specifying images to pre-pull at build time. During `runtime build`, images from all extensions in the runtime are collected and pulled via Docker-in-Docker into the var partition. Multiple extensions can each declare their own images — they're all pulled into a single Docker data root.

```yaml
extensions:
  my-app:
    types: [sysext]
    version: "1.0.0"
    docker_images:
      - image: "docker.io/library/redis"
        tag: "7-alpine"
      - image: "docker.io/library/nginx"
        tag: "1.25"
    sdk:
      packages:
        nativesdk-docker: "*"
```

Minimal config (single image):
```yaml
extensions:
  my-app:
    types: [sysext]
    version: "1.0.0"
    docker_images:
      - image: "docker.io/library/alpine"
        tag: "3.19"
    sdk:
      packages:
        nativesdk-docker: "*"
```

Multiple extensions can each declare Docker images:
```yaml
runtimes:
  dev:
    extensions: [base, app-a, app-b]

extensions:
  app-a:
    types: [sysext]
    version: "1.0.0"
    docker_images:
      - image: "docker.io/library/redis"
        tag: "7-alpine"
  app-b:
    types: [sysext]
    version: "1.0.0"
    docker_images:
      - image: "docker.io/library/nginx"
        tag: "1.25"
```

### Extension var_files (extension-level)

Extensions can declare glob patterns identifying files in their sysroot's `var/` tree to apply to the var partition instead of into the sysext/confext `.raw` image:

```yaml
extensions:
  my-docker-ext:
    types: [sysext]
    version: "1.0.0"
    var_files:
      - "var/lib/docker/**"
      - "var/lib/myapp/data/"
```

Patterns are relative to the extension sysroot directory (`$AVOCADO_EXT_SYSROOTS/<ext_name>`). Matched files are:
- **Excluded** from the `.raw` sysext/confext image during `ext image`
- **Copied** into the runtime's var staging directory during `runtime build`

### Runtime var_files (runtime-level)

Runtimes can specify arbitrary project files to copy into the var partition:

```yaml
runtimes:
  dev:
    extensions: [base]
    var_files:
      - source: "files/var-data/"
        dest: "lib/myapp/"
```

`source` is relative to the project directory. `dest` is relative to the var partition root (`/var`).

## Developer Workflow

```bash
# Build with Docker images pre-cached
avocado ext build                     # build extensions (including var_files)
avocado ext image                     # create .raw images (var_files excluded)
avocado runtime build dev             # build runtime (applies var_files + primes Docker images)
# Output: avocado-image-var-aarch64.btrfs with /var/lib/docker/ pre-populated
```

On target device boot, Docker immediately sees all pre-cached images:
```bash
docker images
# REPOSITORY   TAG        IMAGE ID       SIZE
# redis        7-alpine   abc123...      30MB
# nginx        1.25       def456...      140MB
```

## Pipeline: `runtime build` with Docker Image Priming

The existing `runtime build` pipeline is extended with new steps (marked **NEW**):

1. Resolve extensions, validate stamps (existing)
2. Create `$VAR_DIR` (`var-staging/`) with avocado directory structure (existing)
3. Copy extension `.raw` images to runtime-specific staging (existing)
4. **NEW: Apply extension `var_files`** — Copy matched files from each extension sysroot into `$VAR_DIR`
5. **NEW: Apply runtime `var_files`** — Copy project files into `$VAR_DIR`
6. Generate manifest with content-addressable image IDs (existing)
7. Provision update authority metadata (existing)
8. **NEW: Prime Docker images** — Start temporary dockerd, pull images, stop dockerd
9. `mkfs.btrfs -r "$VAR_DIR"` to create final btrfs image (existing)
10. Run `avocado-build-$TARGET_ARCH` lifecycle hook (existing)

### Extension var_files ordering

When multiple extensions contribute `var_files`, they are applied in reverse order of the runtime's `extensions` list:

```yaml
runtimes:
  dev:
    extensions: [ext-a, ext-b, ext-c]  # ext-a has highest priority
```

- `ext-c` var_files applied first (lowest priority)
- `ext-b` var_files applied second
- `ext-a` var_files applied last (highest priority, wins file conflicts)

This uses `rsync -a` so later copies overwrite earlier ones, giving the first-listed extension the highest priority.

### Docker image priming approach (Docker-in-Docker)

Docker images are primed using Docker-in-Docker inside the SDK container. This is the most portable approach — it works on Linux, macOS (Docker Desktop), and Windows (Docker Desktop) since the inner dockerd always runs inside a Linux container regardless of host OS.

When any extension in the runtime declares `docker_images`, avocado-cli automatically adds `--privileged` to the SDK container invocation so dockerd can run inside the container. Images from all extensions are collected and pulled in a single DinD session.

During `runtime build`, the generated build script:

1. Verifies `dockerd` is available in the SDK container
2. Maps target arch to Docker platform (`aarch64` -> `linux/arm64`, `x86_64` -> `linux/amd64`)
3. Starts a temporary `dockerd` with `--data-root "$VAR_DIR/lib/docker"` and a dedicated unix socket
4. Waits for dockerd readiness (poll loop, 30s timeout)
5. Runs `docker pull --platform linux/$DOCKER_ARCH <image>:<tag>` for each configured image
6. Stops dockerd, cleans up socket and pid files

The result: `$VAR_DIR/lib/docker/` contains Docker's overlay2 storage layout with all images pre-cached. When the target boots, Docker reads `/var/lib/docker/` and finds all images immediately available — no network pull, no first-boot import needed.

**Cross-compilation:** `docker pull --platform linux/arm64` fetches arm64 layers regardless of host architecture. Docker's overlay2 storage format is architecture-independent (filesystem layers + metadata JSON).

**SDK container requirements:**
- Must include `dockerd`, `containerd`, `runc`, and `docker` CLI (e.g., via `nativesdk-docker` packages)
- The build fails with a clear error if `dockerd` is not found
- The SDK container is automatically run with `--privileged` when any extension declares `docker_images`

## Extension var_files exclusion from .raw images

When an extension declares `var_files`, those files must be excluded from the sysext/confext `.raw` image since they belong on the var partition, not in `/usr` or `/etc` overlays.

During `ext image`, the mkfs command receives exclude flags:
- **squashfs:** `mksquashfs ... -e "var/lib/docker" -e "var/lib/myapp/data"`
- **erofs:** `mkfs.erofs ... --exclude-path=var/lib/docker --exclude-path=var/lib/myapp/data`

The files remain in the extension sysroot so `runtime build` can copy them into var staging.

## Implementation Steps

### 1. Config parsing (`src/utils/config.rs`)

- Add `"var_files"` and `"docker_images"` to the known extension keys list to prevent them from being treated as target-specific sections
- Add `"var_files"` to the known runtime keys list
- Add `DockerImageRef` struct with `image: String` and `tag: String` fields
- Add `VarFileMapping` struct with `source: String` and `dest: String` fields
- Add helper functions:
  - `get_ext_var_files(ext_config: &Value) -> Vec<String>` — extracts glob patterns from extension config
  - `get_docker_images(config: &Value) -> Vec<DockerImageRef>` — extracts Docker image references from any config node
  - `get_runtime_var_files(runtime_config: &Value) -> Vec<VarFileMapping>` — extracts source/dest file mappings

### 2. Extension image exclusion (`src/commands/ext/image.rs`)

- Update `create_build_script()` signature to accept `var_files: &[String]`
- Convert glob patterns to mkfs exclude flags (squashfs `-e` / erofs `--exclude-path`)
- Strip trailing `/**` from glob patterns to get directory paths for exclusion
- Update `create_image()` and `execute()` to read `var_files` from extension config and pass through
- Note: `docker_images` does NOT affect extension `.raw` images — Docker data is pulled directly into var staging during `runtime build`, not into the extension sysroot

### 3. Runtime build var_files + Docker priming (`src/commands/runtime/build.rs`)

- In `create_build_script()`, after the existing copy/manifest sections:
  - Generate rsync commands for each extension's `var_files` in reverse extensions-list order
  - Generate rsync commands for runtime-level `var_files`
  - Collect `docker_images` from all extensions in the runtime and generate the Docker priming script section (dockerd start, pull, stop)
- Insert all new sections before the `mkfs.btrfs` command
- When any extension has `docker_images`, automatically add `--privileged` to SDK container args

### 4. Stamp invalidation (`src/utils/stamps.rs`)

- Include `var_files` in `compute_ext_input_hash()` so extension var_files changes invalidate ext image stamps
- Include extension `docker_images` and runtime `var_files` in `compute_runtime_input_hash()` so changes invalidate runtime build stamps

## Critical Files

| File | Change |
|------|--------|
| `src/utils/config.rs` | Add `DockerImageRef`, `VarFileMapping` structs; add known keys; add helper functions |
| `src/commands/ext/image.rs` | Add var_files exclusion to mkfs commands |
| `src/commands/runtime/build.rs` | Add var_files application and Docker priming to build script; collect docker_images from extensions |
| `src/commands/runtime/install.rs` | Pass parsed config to stamp hash computation |
| `src/utils/stamps.rs` | Include extension docker_images and var_files in hash computations |

## Verification

1. `cargo build` — confirms compilation
2. `cargo test` — new unit tests pass, existing tests unchanged
3. Manual test with Docker priming config:
   ```yaml
   extensions:
     my-app:
       types: [sysext]
       version: "1.0.0"
       docker_images:
         - image: "docker.io/library/alpine"
           tag: "3.19"
       sdk:
         packages:
           nativesdk-docker: "*"
   runtimes:
     dev:
       extensions: [base, my-app]
   ```
   ```bash
   avocado runtime build dev
   # Verify: mount the btrfs image and check /var/lib/docker/ contains overlay2 data
   ```
4. Manual test with extension var_files:
   ```yaml
   extensions:
     my-ext:
       types: [sysext]
       version: "1.0.0"
       var_files:
         - "var/lib/myapp/**"
   ```
   ```bash
   avocado ext image my-ext
   # Verify: .raw image does NOT contain var/lib/myapp/
   avocado runtime build dev
   # Verify: btrfs image contains /var/lib/myapp/ files from extension sysroot
   ```
