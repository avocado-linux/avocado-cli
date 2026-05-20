# Avocado CLI

Command line interface for Avocado.

- [Documentation](https://docs.peridio.com/developer-reference/avocado-cli/overview)

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

## Managing the avocado-vm

The CLI launches and updates a host-side helper VM ([`avocado-linux/avocado-vm`](https://github.com/avocado-linux/avocado-vm)) for `dockerd`, USB pass-through, and project mounts.

### Updating the VM image

```bash
# Check for a newer release on the configured channel (default: stable):
avocado vm update --check

# Apply it. Stops the VM during the swap and restarts it if it was running.
# Preserves the existing /var (Docker volumes, container caches, project work
# under /data) — only kernel/initramfs/rootfs are replaced.
avocado vm update

# Scriptable / unattended:
avocado vm update -y
avocado vm update --output json
```

The CLI polls `https://repo.avocadolinux.org/releases/vm/<channel>.json` on a 24-hour cache. Set `AVOCADO_NO_UPDATE_CHECK=1` to disable polling, or use `--channel <name>` to override the default channel for a single invocation.

To pin the channel, create `~/.avocado/config.yaml`:

```yaml
vm:
  channel: beta
```

### Resetting VM state

```bash
avocado vm reset
```

Wipes the persistent `var.btrfs` and re-seeds it from the installed var artifact. Use this when `/var` is corrupted, you want to test a provisioning flow from scratch, or accumulated state has gotten in the way. Doesn't change the VM image version — that's what `vm update` is for.

Requires typing `reset` (not just `y`) to confirm — this drops Docker volumes, container caches, `/etc/machine-id`, and anything else under `/var` or `/data`. Add `-y` to skip the prompt in scripts.
