#!/usr/bin/env bash
#
# Build the Container Dev Mode demo app on the lab VM's engine and install the
# systemd unit that owns its container.
#
# Replaces two hand-run steps that were easy to get wrong:
#
#   1. Building the image. A bare `docker build` uses whatever DOCKER_HOST the
#      shell happens to carry. In a terminal that never sourced env.sh that is the
#      HOST daemon, so the image lands on the developer's machine, the VM never
#      sees it, and the reload silently does nothing. This script pins the guest
#      daemon itself and refuses to run if it cannot reach it.
#
#   2. Writing the unit. The container must be owned by a systemd unit whose
#      ExecStart re-resolves the TAG (`docker run`), not one that restarts a
#      container: an engine `restart` re-runs the image ID pinned at create time,
#      so a freshly pulled image for the same tag would never actually run. The
#      device agent restarts this unit when AVOCADO_CONTAINER_DEV_SERVICE names it.
#
# The unit name matches the `service:` field under `container_dev.images` in the
# lab's avocado.yaml, which is what the host tells the device to restart.
#
# Usage:
#   docs/container-dev/lab/install-demo-app.sh [version-string]
#   BUILD_ONLY=1 docs/container-dev/lab/install-demo-app.sh v2   # rebuild only
#
# BUILD_ONLY=1 skips installing and restarting the unit, so the ONLY thing that can
# move the running container to the new image is the watcher -> push -> notify ->
# agent path. Use it to trigger a reload; restarting the unit here would adopt the
# image directly and prove nothing about the loop.
#
# Environment:
#   SSH_ALIAS   ssh alias for the lab VM        (default: avocado-vm-lab)
#   DOCK_SOCK   forwarded guest docker socket   (default: ~/.avocado/vm/docker.sock)
#   TEST_IMAGE  watched image ref               (default: my-app:dev)
#   APP_SERVICE systemd unit to own it          (default: app.service)
#   BUILD_CTX   build context dir               (default: /tmp/cdm-app)
#   BUILD_ONLY  skip unit install/restart       (default: unset)

set -euo pipefail

VERSION="${1:-v1}"
BUILD_ONLY="${BUILD_ONLY:-}"
SSH_ALIAS="${SSH_ALIAS:-avocado-vm-lab}"
DOCK_SOCK="${DOCK_SOCK:-$HOME/.avocado/vm/docker.sock}"
TEST_IMAGE="${TEST_IMAGE:-my-app:dev}"
APP_SERVICE="${APP_SERVICE:-app.service}"
BUILD_CTX="${BUILD_CTX:-/tmp/cdm-app}"
CONTAINER="${APP_SERVICE%.service}"

say() { echo ">> $*"; }

# Pin the guest daemon rather than inheriting whatever the shell carries.
export DOCKER_HOST="unix://$DOCK_SOCK"

[ -S "$DOCK_SOCK" ] || {
  echo "no forwarded guest docker socket at $DOCK_SOCK - run setup-lab.sh first" >&2
  exit 1
}

daemon="$(docker info --format '{{.Name}}' 2>/dev/null || true)"
[ "$daemon" = "$SSH_ALIAS" ] || {
  echo "docker socket $DOCK_SOCK answers as '$daemon', expected '$SSH_ALIAS'" >&2
  echo "refusing to build: the image would land on the wrong daemon and the device would never see it" >&2
  exit 1
}
say "guest engine: $daemon"

# 1. Build context. An observable version plus a large, unchanging base layer, so
#    a rebuild moves one small layer and the delta is visible in the push output.
say "writing build context to $BUILD_CTX (version $VERSION)"
mkdir -p "$BUILD_CTX"
cat >"$BUILD_CTX/Dockerfile" <<EOF
FROM busybox:latest
RUN yes avocado | head -c 524288 > /base.bin
RUN printf '$VERSION\\n' > /version
CMD ["sh","-c","while true; do echo \\"app \$(cat /version) base=\$(wc -c </base.bin)B\\"; sleep 2; done"]
EOF

# 2. Build on the guest engine. DOCKER_BUILDKIT=0 because BuildKit emits no image
#    tag event, so the watcher would never see the rebuild (the classic builder
#    does emit one). `container dev sync` is the BuildKit-safe alternative.
say "building $TEST_IMAGE on the guest engine (classic builder, so the watcher sees the tag)"
DOCKER_BUILDKIT=0 docker build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null

if [ -n "$BUILD_ONLY" ]; then
  say "BUILD_ONLY set: leaving $APP_SERVICE alone so the reload can only come from the watcher"
  say "watch it land: docker logs --tail 1 ${CONTAINER}   (and tail the \`container dev up\` log)"
  exit 0
fi

# 3. Install the owning unit on the device.
say "installing $APP_SERVICE on $SSH_ALIAS"
# SC2087: local expansion is intended - the unit must be written with the image
# ref and container name resolved here, not left as literals for the device shell.
# shellcheck disable=SC2087
ssh "$SSH_ALIAS" "cat > /etc/systemd/system/$APP_SERVICE" <<EOF
[Unit]
Description=Container Dev Mode demo app
Requires=docker.service
After=docker.service

[Service]
# Recreate the container from the TAG on every start so a freshly pulled image for
# that tag is adopted. An engine 'restart' would re-run the image ID pinned at
# create time and the reload would silently no-op.
ExecStartPre=-/usr/bin/docker rm -f $CONTAINER
ExecStart=/usr/bin/docker run --rm --name $CONTAINER $TEST_IMAGE
ExecStop=-/usr/bin/docker stop $CONTAINER
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF

ssh "$SSH_ALIAS" "systemctl daemon-reload && systemctl enable --now $APP_SERVICE >/dev/null 2>&1; systemctl restart $APP_SERVICE"

# 4. Prove it is actually running the version we just built.
sleep 4
line="$(docker logs --tail 1 "$CONTAINER" 2>&1 || true)"
say "app says: $line"
case "$line" in
  *"$VERSION"*) say "demo app ready on the device" ;;
  *)
    echo "app is not reporting version '$VERSION' - check: ssh $SSH_ALIAS 'journalctl -u $APP_SERVICE -n 20'" >&2
    exit 1
    ;;
esac
