# Container Dev Mode - local VM write-path lab

A from-scratch harness to exercise Container Dev Mode's authenticated VM write
path (task 7.1) on Linux, without hardware. It stands up a disposable Debian 12
"engine VM" under QEMU user-mode networking so the guest reaches the host at
`10.0.2.2` exactly like the macOS `avocado-vm`, then runs the end-to-end verify.

This is the setup the 2026-07-23 field note ("Container Dev Mode VM push") was
written from; it validated the path 8/8 and caught two real bugs (a plain-HTTP
write listener where the guest required HTTPS, and a 2 MiB body limit that
413'd real layers).

## What's here

- `setup-lab.sh` - idempotent provisioner: ssh keypair, cloud-init seed
  (`docker.io` + root login), a copy-on-write overlay off the Debian base, a
  QEMU SLIRP boot with an ssh hostfwd, a forward of the guest dockerd to the
  socket `is_vm_routing_active()` resolves, and an `env.sh` for the verify step.
- `avocado.yaml` - the minimal `container_dev` runtime config the lab uses.
- `../verify-vm-write-path.sh` - the actual end-to-end assertion (built image ->
  authenticated push over `10.0.2.2` HTTPS -> single-layer sync). Sourced env
  comes from the generated `env.sh`.

Generated state (the qcow2 overlay, ssh key, cloud-init seed, `env.sh`) is
written to a work dir OUTSIDE this repo (`$AVOCADO_CDM_LAB_WORK`, default
`~/.cache/avocado-cdm-lab`) so a ~900 MB overlay never lands in git.

## Prerequisites

Host packages: `qemu-system-x86_64`, `qemu-img`, `cloud-image-utils`
(`cloud-localds`), `ssh`/`ssh-keygen`, and the `docker` client.

## Run it (from scratch)

```bash
# 1. one-time: download a Debian 12 generic-cloud base image
WORK="${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
mkdir -p "$WORK"
curl -L -o "$WORK/debian12.qcow2" \
  https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2

# 2. build the avocado CLI (the lab points AVOCADO_BIN at target/debug/avocado)
cargo build --bin avocado

# 3. stand up the engine VM (interactive: it runs ssh-keygen + touches ~/.ssh)
bash docs/container-dev/lab/setup-lab.sh

# 4. run the end-to-end verify
source "$WORK/env.sh"
docs/container-dev/verify-vm-write-path.sh
```

Optional: set `BBAPPEND` to the meta-avocado
`meta-avocado-qemu/recipes-core/base-files/base-files_%.bbappend` path so the
verify script also checks the guest trust-store-dir overlay; leave it empty to
skip that check.

## Tear down

```bash
WORK="${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
kill "$(cat "$WORK/qemu.pid")" 2>/dev/null || true
pkill -f "$HOME/.avocado/vm/docker.sock:" 2>/dev/null || true
```

Deleting `$WORK/engine.qcow2` gives a clean VM on the next run; the base image
and ssh key are reused.
