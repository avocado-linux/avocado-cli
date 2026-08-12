#!/usr/bin/env bash
#
# setup-lab.sh - Stand up a real Avocado OS HITL target for Container Dev Mode.
#
# The target is Avocado OS, because Avocado OS is the OS the feature ships on and
# every board is expected to run it. This script previously booted a Debian cloud
# image as a stand-in; that stand-in produced a false finding (its docker 20.10.24
# emits no tag event for BuildKit builds, which was generalised into a property of
# BuildKit rather than of that daemon), so it is gone. Do not reintroduce it.
#
# What it does, all idempotent:
#
#   1. Renders a runtime config into $WORK/hitl and builds it from the published
#      2024/edge feed plus the SDK container. Two extensions matter:
#        avocado-ext-docker              - published in the feed.
#        avocado-ext-container-agent-dev - NOT published; sourced from the local
#                                          avocado-os checkout and compiled by the
#                                          SDK for x86_64-avocado-linux-gnu.
#   2. Provisions it with the default `img` profile (fwup: gpt_write + raw_write).
#      The `direct` profile is NOT the path - its own header says "No fwup archive,
#      no GPT, no A/B slots, no bootloader", which is why a hand-rolled direct boot
#      has no GPT partition UUID for /var to wait on and lands in emergency mode.
#   3. Copies the disk image and u-boot.rom out of the SDK docker volume onto the
#      host, and boots them under host QEMU. Running on the host rather than inside
#      the SDK container is deliberate: the target has to be a genuinely separate
#      machine reached only over ssh, which is the entire point of the topology.
#   4. Adds an ssh alias, and installs the agent drop-in the device needs.
#   5. Writes an env file the demo driver and verify script source.
#
# Prerequisites on the host: docker (for the SDK container), qemu-system-x86_64,
# qemu-img, ssh, python3, an `avocado` on PATH carrying `container dev`, and a
# checkout of avocado-os on a branch that has extensions/container-agent-dev.
#
# Run it yourself (it touches ~/.ssh/config, so run interactively, not from an
# agent):
#   bash docs/container-dev/lab/setup-lab.sh
# Then:
#   source "${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}/env.sh"
#   docs/container-dev/lab/demo.sh all
#
# Tunables (env overrides): AVOCADO_CDM_LAB_WORK (generated-state dir),
# AVOCADO_CLI (avocado-cli repo root), AVOCADO_OS (avocado-os repo root),
# TARGET (avocado target, default qemux86-64), DISK_SIZE, SSH_PORT, MEM, SMP.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Generated state (disk image, rendered config, env.sh) lives OUTSIDE the repo
# checkout so a ~1 GB image never lands in git.
WORK="${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
# This script sits at docs/container-dev/lab/, so ../../.. is the crate root.
AVOCADO_CLI="${AVOCADO_CLI:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
# avocado-os is a sibling checkout of avocado-cli in the peridio workspace.
AVOCADO_OS="${AVOCADO_OS:-$(cd "$AVOCADO_CLI/.." && pwd)/avocado-os}"

TARGET="${TARGET:-qemux86-64}"
PROJ="$WORK/hitl"
VMDIR="$WORK/hitl-vm"
IMG="$VMDIR/avocado-os-$TARGET.img"
BIOS="$VMDIR/u-boot.rom"
DISK_SIZE="${DISK_SIZE:-8192M}"
SSH_PORT="${SSH_PORT:-2222}"
MEM="${MEM:-2048}"
SMP="${SMP:-2}"
SSH_ALIAS="${SSH_ALIAS:-avocado-hitl}"
APP_SERVICE="${APP_SERVICE:-app.service}"
CONSOLE="$VMDIR/console.log"
PIDFILE="$VMDIR/qemu.pid"
# is_vm_routing_active() keys on exactly this socket path, so MODE=vm needs it here.
VMROOT="$HOME/.avocado/vm"
DOCK_SOCK="$VMROOT/docker.sock"
WRITE_PORT=5601

say() { echo ">> $*"; }
die() { echo "$*" >&2; exit 1; }

AGENT_EXT="$AVOCADO_OS/extensions/container-agent-dev"
[ -d "$AGENT_EXT" ] || die "missing $AGENT_EXT - set AVOCADO_OS to an avocado-os checkout carrying extensions/container-agent-dev"

command -v avocado >/dev/null || die "no 'avocado' on PATH"
avocado container dev --help >/dev/null 2>&1 \
  || die "the 'avocado' on PATH has no 'container dev' subcommand - rebuild it from the working branch"

mkdir -p "$PROJ" "$VMDIR"

# ---------------------------------------------------------------------------
# 1. Render the runtime config.
#
# Generated rather than tracked because the container-agent-dev extension is
# sourced by ABSOLUTE path - it is not in the published feed, so it has to point
# at wherever avocado-os is checked out on this machine.
# ---------------------------------------------------------------------------
say "rendering $PROJ/avocado.yaml (target $TARGET, agent ext from $AGENT_EXT)"
# QUOTED heredoc, with @PLACEHOLDER@ substituted afterwards.
#
# It used to be unquoted so $TARGET and $AGENT_EXT would expand, which also made
# every backtick in the prose below a command substitution. That shipped: a comment
# mentioning `docker build ...` and `x509: ...` ran both as commands on every run,
# printing "requires 1 argument" and "x509:: command not found" and silently
# emptying the text from the generated file. Quoting the delimiter makes the whole
# block inert, so no future comment can execute, and the two values that genuinely
# vary are injected explicitly below where they are easy to see.
cat >"$PROJ/avocado.yaml" <<'EOF'
# GENERATED by setup-lab.sh - edit the generator, not this file.
#
# Shape follows the published qemu-quickstart reference, minus connect/tunnels
# (they need org credentials this lab has no use for), plus the two extensions
# Container Dev Mode requires.
default_target: @TARGET@

supported_targets:
  - @TARGET@

distro:
  release: 2024
  channel: edge

runtimes:
  dev:
    extensions:
      - avocado-ext-dev
      - avocado-ext-sshd-dev
      - avocado-bsp-{{ avocado.target.board }}
      - avocado-ext-ca-certificates
      - avocado-ext-docker
      - avocado-ext-container-agent-dev
      - config
    packages:
      avocado-runtime: "*"

extensions:
  avocado-ext-dev:
    source: {type: package, version: "*"}

  # Public root CAs. Without these the target's engine cannot verify TLS to any
  # public registry: a guest-side `docker build FROM busybox:latest` dies with
  # `x509: certificate signed by unknown authority`, which is what took Part A to
  # 7/8. Container Dev Mode's own traffic does not need this - it pins the
  # per-project CA - but anything that pulls a public base image does.
  avocado-ext-ca-certificates:
    source: {type: package, version: "*"}

  avocado-ext-sshd-dev:
    source: {type: package, version: "*"}

  avocado-bsp-{{ avocado.target.board }}:
    source: {type: package, version: "*"}

  avocado-ext-docker:
    source: {type: package, version: "*"}

  # Not published in the feed - built from source by the SDK via the extension's
  # own cad-compile.sh / cad-install.sh, which target x86_64-avocado-linux-gnu.
  avocado-ext-container-agent-dev:
    source:
      type: path
      path: @AGENT_EXT@

  # Empty root password so the lab can ssh in without provisioning a key.
  # Dev target only - this is what avocado-ext-sshd-dev exists for.
  config:
    types:
      - confext
    version: "0.1.0"
    users:
      root:
        password: ""

sdk:
  image: "docker.io/avocadolinux/sdk:{{ avocado.distro.release }}-{{ avocado.distro.channel }}"
  container_args:
    - --privileged
    - --network=host
    - -v /dev:/dev
    - -v /sys:/sys
  packages:
    avocado-sdk-toolchain: "*"
EOF

# Inject the two values the template leaves open. `|` as the delimiter because
# AGENT_EXT is a path and would otherwise need its slashes escaped.
sed -i \
  -e "s|@TARGET@|$TARGET|g" \
  -e "s|@AGENT_EXT@|$AGENT_EXT|g" \
  "$PROJ/avocado.yaml"

# Fail loudly rather than handing avocado a config with an unfilled slot.
if grep -q '@[A-Z_]\+@' "$PROJ/avocado.yaml"; then
  die "unsubstituted placeholder left in $PROJ/avocado.yaml: $(grep -o '@[A-Z_]\+@' "$PROJ/avocado.yaml" | sort -u | tr '\n' ' ')"
fi

# ---------------------------------------------------------------------------
# 2. Build + provision.
#
# DOCKER_HOST must be unset for all of these: the SDK container runs on the HOST
# daemon. A DOCKER_HOST left pointing at the target's socket sends the build to
# the wrong daemon, and dockerd then auto-creates the missing bind source as an
# empty directory - which surfaces as a baffling "could not find Cargo.toml".
# ---------------------------------------------------------------------------
if [ ! -f "$IMG" ]; then
  say "installing SDK + extension deps (first run pulls the SDK image: minutes)"
  ( cd "$PROJ" && env -u DOCKER_HOST avocado install -f )

  say "building the runtime (compiles the agent for the target ABI)"
  ( cd "$PROJ" && env -u DOCKER_HOST avocado build )

  say "provisioning with the default 'img' profile"
  ( cd "$PROJ" && env -u DOCKER_HOST avocado provision -f dev )

  # 3. Copy the image + BIOS out of the SDK volume onto the host.
  say "copying the disk image and u-boot.rom out of the SDK volume"
  vol="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["volume_name"])' "$PROJ/.avocado-state")"
  stone="/opt/_avocado/$TARGET/output/runtimes/dev/stone"
  env -u DOCKER_HOST docker run --rm \
    -v "$vol":/opt/_avocado -v "$VMDIR":/out alpine:3 sh -c "
      set -e
      cp $stone/_build/avocado-os-$TARGET.img /out/
      cp $stone/u-boot.rom /out/
      chown -R $(id -u):$(id -g) /out
    "
else
  say "image already present at $IMG (delete it to rebuild from scratch)"
fi

# ---------------------------------------------------------------------------
# 4. Boot it.
# ---------------------------------------------------------------------------
if [ -f "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
  say "HITL target already running (pid $(cat "$PIDFILE"))"
else
  # Grow only, never shrink. The SDK's own vm script runs an unconditional
  # `qemu-img resize -f raw <img> 1024M`, which is a SHRINK for any image over
  # 1024M; qemu-img refuses it and the script dies under `set -e` before qemu
  # starts. Growing also gives /var room for the container images the demo pulls
  # (avocado-grow-var.service expands /var to fill the disk on boot).
  cur="$(qemu-img info --output=json "$IMG" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["virtual-size"])')"
  want="$(numfmt --from=iec "${DISK_SIZE%B}")"
  if [ "$cur" -lt "$want" ]; then
    say "growing the disk to $DISK_SIZE"
    qemu-img resize -f raw "$IMG" "$DISK_SIZE"
  fi

  # TCG, not KVM: `-cpu host -enable-kvm` faults the u-boot BIOS with
  # "Exception 13 executing option rom". The disk attaches as an SD card
  # (sdhci-pci + sd-card) because that is where this u-boot looks for its boot
  # partition. The guest reaches the host at 10.0.2.2 under SLIRP, which is how
  # the agent dials the host's control WS and registry - only ssh needs a
  # hostfwd, every Container Dev Mode connection is guest-initiated.
  say "booting the HITL target (TCG, ssh hostfwd $SSH_PORT->22, console -> $CONSOLE)"
  qemu-system-x86_64 \
    -bios "$BIOS" \
    -device sdhci-pci -device sd-card,drive=mmc \
    -drive file="$IMG",if=none,format=raw,id=mmc \
    -m "$MEM" -smp "$SMP" -cpu max \
    -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:${SSH_PORT}-:22" \
    -device e1000,netdev=net0 \
    -display none -serial file:"$CONSOLE" -monitor none \
    -daemonize -pidfile "$PIDFILE"
fi

# ---------------------------------------------------------------------------
# 5. ssh alias, PREPENDED so its host-key policy wins.
#
# ssh uses the FIRST value seen for each keyword, so a global "Host *" block
# earlier in the file would otherwise force its StrictHostKeyChecking and
# UserKnownHostsFile onto this alias. Our block at the top makes accept-new +
# /dev/null win, so a throwaway target whose host key changes on every reprovision
# never triggers a refusal.
# ---------------------------------------------------------------------------
mkdir -p "$HOME/.ssh"
CFG="$HOME/.ssh/config"
touch "$CFG"
say "prepending ssh alias '$SSH_ALIAS' to ~/.ssh/config"
STRIPPED="$(awk '
  /^Host '"$SSH_ALIAS"'$/ {skip=1; next}
  skip && /^[ \t]/ {next}
  {skip=0; print}
' "$CFG")"
# No backticks in the heredoc below: it is unquoted so the $VARs expand, which
# means a backtick would run as command substitution on the host instead of
# landing as text. (That bug shipped once and ran `config` as a command.)
{
  cat <<EOF
Host $SSH_ALIAS
  HostName 127.0.0.1
  Port $SSH_PORT
  User root
  StrictHostKeyChecking accept-new
  UserKnownHostsFile /dev/null
  # The dev target's root password is empty by design (the config confext sets
  # it), and avocado-ext-sshd-dev permits it. No key is provisioned, so keep
  # password auth available and do not let an agent-forwarded key be tried first.
  PreferredAuthentications password,keyboard-interactive
  PubkeyAuthentication no

EOF
  printf '%s\n' "$STRIPPED"
} >"$CFG"
chmod 600 "$CFG"

say "waiting for ssh + docker on the target (first boot ~60-90s under TCG)"
ok=0
for _ in $(seq 1 60); do
  if ssh -o ConnectTimeout=3 "$SSH_ALIAS" 'docker version >/dev/null 2>&1' 2>/dev/null; then
    ok=1
    break
  fi
  sleep 3
done
[ "$ok" = 1 ] || { echo "target never became ready; see $CONSOLE" >&2; exit 1; }
say "target ready: ssh + docker"

# ---------------------------------------------------------------------------
# 6. Agent drop-in.
#
# The agent learns which unit owns the container ONLY from
# $AVOCADO_CONTAINER_DEV_SERVICE (agent/src/sync.rs service_from_env). The
# `service:` field under container_dev.images is host-side config: DeviceBootstrap
# carries bulk_endpoint, read_token, ca_cert_pem and ws_endpoint - no service - so
# nothing delivers it to the device. Without this drop-in the agent falls back to
# `docker restart <container>`, which re-executes the container's pinned image ID,
# so a freshly pulled image is ignored and every sync silently no-ops while
# reporting success. Install it here rather than leaving it to the operator.
# ---------------------------------------------------------------------------
say "installing the agent drop-in (AVOCADO_CONTAINER_DEV_SERVICE=$APP_SERVICE)"
# shellcheck disable=SC2087  # client-side expansion is intended: bake APP_SERVICE in
# mkdir -p, not install -d: the target's coreutils are BusyBox and it has no
# `install`. Anything this script runs on the target must stay inside BusyBox's
# subset (same reason `head -n N` is required over `head -N`).
ssh "$SSH_ALIAS" "mkdir -p /etc/systemd/system/container-agent-dev.service.d && \
  cat > /etc/systemd/system/container-agent-dev.service.d/10-service.conf" <<EOF
# Installed by setup-lab.sh. See the note in that script: nothing in the
# bootstrap payload carries the owning unit name, so it must be set here or the
# agent's restart silently no-ops on the pinned image ID.
[Service]
Environment=AVOCADO_CONTAINER_DEV_SERVICE=$APP_SERVICE
EOF
ssh "$SSH_ALIAS" 'systemctl daemon-reload'

# ---------------------------------------------------------------------------
# 7. Forward the target's dockerd to the socket is_vm_routing_active() resolves.
#
# Only MODE=vm and verify-vm-write-path.sh use this. It is safe to leave in place
# for native mode because the CLI's vm path keys on DOCKER_HOST matching this
# socket, NOT on the socket existing - so an unset DOCKER_HOST still takes the
# native path (which is why env.sh deliberately does not export it).
# ---------------------------------------------------------------------------
say "forwarding the target's dockerd -> $DOCK_SOCK"
mkdir -p "$VMROOT"
pkill -f "${DOCK_SOCK}:/var/run/docker.sock" 2>/dev/null || true
rm -f "$DOCK_SOCK"
ssh -f -N -L "${DOCK_SOCK}:/var/run/docker.sock" "$SSH_ALIAS"
for _ in $(seq 1 10); do
  [ -S "$DOCK_SOCK" ] && break
  sleep 1
done
if DOCKER_HOST="unix://$DOCK_SOCK" docker version >/dev/null 2>&1; then
  say "target engine reachable via $DOCK_SOCK"
else
  die "target engine not reachable via $DOCK_SOCK"
fi

# ---------------------------------------------------------------------------
# 8. env file for the demo driver and the verify script.
# ---------------------------------------------------------------------------
AVOCADO_BIN="${AVOCADO_BIN:-$(command -v avocado)}"
TARGET_HOSTNAME="$(ssh "$SSH_ALIAS" 'hostname' 2>/dev/null || echo "$SSH_ALIAS")"

cat >"$WORK/env.sh" <<EOF
# source this before running docs/container-dev/lab/demo.sh
export AVOCADO_BIN=$AVOCADO_BIN
export AVOCADO_CDM_LAB_WORK=$WORK
export SSH_ALIAS=$SSH_ALIAS
# The target's docker daemon reports its own hostname, which is the image's
# hostname and NOT the ssh alias - the demo driver compares against this.
export TARGET_HOSTNAME=$TARGET_HOSTNAME
export AVOCADO_CONTAINER_DEV_VM=$SSH_ALIAS
export AVOCADO_CONTAINER_DEV_DEVICE=$SSH_ALIAS
# Under SLIRP user networking the guest always sees the host as 10.0.2.2.
export AVOCADO_CONTAINER_DEV_HOST=10.0.2.2
export AVOCADO_CONTAINER_DEV_WRITE_PORT=$WRITE_PORT
export TEST_IMAGE=my-app:dev
export APP_SERVICE=$APP_SERVICE
export AVOCADO_CONFIG=$SCRIPT_DIR/avocado.yaml
# NOTE: DOCKER_HOST is deliberately NOT exported. Native mode - the host builds,
# the target only runs - is the default topology, and is_vm_routing_active() keys
# solely on DOCKER_HOST matching the avocado-vm socket. Exporting it here would
# silently switch the CLI to the vm path. MODE=vm sets it up on demand.
EOF

echo
say "lab is up. next:"
echo "   source $WORK/env.sh"
echo "   $SCRIPT_DIR/demo.sh all"
echo ">> to tear down: $SCRIPT_DIR/demo.sh reset"
