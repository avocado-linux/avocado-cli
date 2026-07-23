#!/usr/bin/env bash
#
# setup-lab.sh - Stand up a local docker+ssh "engine VM" to exercise Container
# Dev Mode's authenticated VM write path (task 7.1) on Linux, from scratch.
#
# It provisions a generic Debian 12 VM under QEMU user-mode networking (so the
# guest reaches the host at 10.0.2.2, exactly like the macOS avocado-vm),
# forwards the guest dockerd to the socket the CLI's is_vm_routing_active()
# looks for, and writes an env file the verify script sources. Idempotent:
# re-running reuses the key, ssh alias, COW overlay, and a live VM.
#
# Prerequisites on the host: qemu-system-x86_64, qemu-img, cloud-image-utils
# (cloud-localds), ssh, ssh-keygen, docker (client only, to talk to the socket).
#
# One-time: download a Debian 12 generic-cloud base image into the work dir as
# debian12.qcow2 (this is the only artifact not generated here):
#   mkdir -p "${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
#   curl -L -o "${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}/debian12.qcow2" \
#     https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2
#
# Run it yourself (it does ssh-keygen + touches ~/.ssh, so run interactively,
# not from an agent):
#   bash docs/container-dev/lab/setup-lab.sh
# Then:
#   source "${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}/env.sh"
#   docs/container-dev/verify-vm-write-path.sh
#
# Tunables (env overrides): AVOCADO_CDM_LAB_WORK (generated-state dir),
# AVOCADO_CLI (avocado-cli repo root), AVOCADO_CDM_BASE_IMG (base qcow2),
# BBAPPEND (meta-avocado base-files bbappend, for the verify overlay check).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Generated state (qcow2s, key, seed, env.sh) lives OUTSIDE the repo checkout so
# a ~900 MB overlay never lands in git. Override with AVOCADO_CDM_LAB_WORK.
WORK="${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
# avocado-cli repo root: this script sits at docs/container-dev/lab/, so ../../..
# is the crate root. Override with AVOCADO_CLI when running from elsewhere.
AVOCADO_CLI="${AVOCADO_CLI:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
# The meta-avocado base-files bbappend the verify script's overlay check targets
# (task 7.1 deliverable). Optional: empty skips that check in the verify script.
BBAPPEND="${BBAPPEND:-}"

mkdir -p "$WORK"
KEY="$WORK/id_lab"
BASE_IMG="${AVOCADO_CDM_BASE_IMG:-$WORK/debian12.qcow2}"
SEED="$WORK/seed.iso"
DISK="$WORK/engine.qcow2"
SSH_PORT=2222
SSH_ALIAS=avocado-vm-lab
VM_USER=root  # deliver_vm_ca writes /etc/docker/certs.d, matching the real avocado-vm's root login
VMROOT="$HOME/.avocado/vm"
DOCK_SOCK="$VMROOT/docker.sock"
WRITE_PORT=5601

say() { echo ">> $*"; }

[ -f "$BASE_IMG" ] || {
  echo "missing base image $BASE_IMG - download a Debian 12 generic-cloud qcow2 there first (see the header)" >&2
  exit 1
}

# 1. SSH key (once)
if [ ! -f "$KEY" ]; then
  say "generating lab ssh key $KEY"
  ssh-keygen -t ed25519 -f "$KEY" -N '' -C avocado-cdm-lab
fi
PUB="$(cat "$KEY.pub")"

# 2. ssh config alias, PREPENDED so its host-key policy wins. ssh uses the FIRST
#    value seen for each keyword; a global "Host *" block earlier in the file
#    would otherwise force its StrictHostKeyChecking/UserKnownHostsFile onto this
#    alias. Putting our block at the top makes accept-new + /dev/null win for both
#    these scripts and the CLI (which inherits UserKnownHostsFile from config), so
#    a throwaway VM whose host key changes on re-provision never triggers a refusal.
mkdir -p "$HOME/.ssh"
CFG="$HOME/.ssh/config"
touch "$CFG"
say "prepending ssh alias '$SSH_ALIAS' to ~/.ssh/config"
STRIPPED="$(awk '
  /^Host '"$SSH_ALIAS"'$/ {skip=1; next}
  skip && /^[ \t]/ {next}
  {skip=0; print}
' "$CFG")"
{
  cat <<EOF
Host $SSH_ALIAS
  HostName 127.0.0.1
  Port $SSH_PORT
  User $VM_USER
  IdentityFile $KEY
  IdentitiesOnly yes
  StrictHostKeyChecking accept-new
  UserKnownHostsFile /dev/null

EOF
  printf '%s\n' "$STRIPPED"
} >"$CFG"
chmod 600 "$CFG"

# 3. cloud-init seed: docker + our key on root (root login mirrors the real
#    avocado-vm engine, so deliver_vm_ca can write /etc/docker/certs.d).
say "building cloud-init seed"
cat >"$WORK/user-data" <<EOF
#cloud-config
disable_root: false
users:
  - name: $VM_USER
    ssh_authorized_keys:
      - $PUB
package_update: true
packages:
  - docker.io
runcmd:
  - systemctl enable --now docker
  - install -d /var/lib/avocado
EOF
printf 'instance-id: avocado-cdm-lab\nlocal-hostname: avocado-vm-lab\n' >"$WORK/meta-data"
cloud-localds "$SEED" "$WORK/user-data" "$WORK/meta-data"

# 4. copy-on-write overlay off the base image (delete engine.qcow2 for a clean VM)
if [ ! -f "$DISK" ]; then
  say "creating cow overlay $DISK"
  qemu-img create -f qcow2 -b "$BASE_IMG" -F qcow2 "$DISK" 20G >/dev/null
fi

# 5. boot the VM if it is not already answering ssh
if ssh -o ConnectTimeout=3 "$SSH_ALIAS" true 2>/dev/null; then
  say "VM already up (ssh answers)"
else
  say "booting the engine VM under QEMU (SLIRP net, ssh hostfwd $SSH_PORT->22)"
  ACCEL=()
  [ -w /dev/kvm ] && ACCEL=(-enable-kvm -cpu host)
  qemu-system-x86_64 "${ACCEL[@]}" -m 2048 -smp 2 \
    -drive file="$DISK",if=virtio \
    -drive file="$SEED",if=virtio,format=raw \
    -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22" \
    -device virtio-net-pci,netdev=n0 \
    -display none -serial file:"$WORK/console.log" -monitor none \
    -daemonize -pidfile "$WORK/qemu.pid"

  say "waiting for ssh + docker (first boot installs docker.io, ~1-3 min)"
  ok=0
  for _ in $(seq 1 120); do
    if ssh -o ConnectTimeout=3 "$SSH_ALIAS" 'docker version >/dev/null 2>&1' 2>/dev/null; then
      ok=1
      break
    fi
    sleep 3
  done
  [ "$ok" = 1 ] || {
    echo "VM never became ready; see $WORK/console.log" >&2
    exit 1
  }
fi
say "VM ready: ssh + docker"

# 6. forward guest dockerd -> the socket is_vm_routing_active() resolves
say "forwarding guest dockerd -> $DOCK_SOCK"
mkdir -p "$VMROOT"
pkill -f "${DOCK_SOCK}:/var/run/docker.sock" 2>/dev/null || true
rm -f "$DOCK_SOCK"
ssh -f -N -L "${DOCK_SOCK}:/var/run/docker.sock" "$SSH_ALIAS"
for _ in $(seq 1 10); do
  [ -S "$DOCK_SOCK" ] && break
  sleep 1
done
if DOCKER_HOST="unix://$DOCK_SOCK" docker version >/dev/null 2>&1; then
  say "DOCKER_HOST socket live"
else
  echo "docker not reachable via $DOCK_SOCK" >&2
  exit 1
fi

# 7. env file for the verify script
cat >"$WORK/env.sh" <<EOF
# source this before running docs/container-dev/verify-vm-write-path.sh
export AVOCADO_BIN=$AVOCADO_CLI/target/debug/avocado
export DOCKER_HOST=unix://$DOCK_SOCK
export AVOCADO_CONTAINER_DEV_VM=$SSH_ALIAS
export AVOCADO_CONTAINER_DEV_DEVICE=$SSH_ALIAS
export AVOCADO_CONTAINER_DEV_HOST=10.0.2.2
export AVOCADO_CONTAINER_DEV_WRITE_PORT=$WRITE_PORT
export TEST_IMAGE=my-app:dev
export AVOCADO_CONFIG=$SCRIPT_DIR/avocado.yaml
export BBAPPEND=$BBAPPEND
EOF

echo
say "lab is up. next:"
echo "   source $WORK/env.sh"
echo "   $AVOCADO_CLI/docs/container-dev/verify-vm-write-path.sh"
echo ">> to tear down: kill \$(cat $WORK/qemu.pid) ; pkill -f '${DOCK_SOCK}:'"
