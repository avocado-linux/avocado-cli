#!/usr/bin/env bash
#
# Container Dev Mode demo driver - one entry point for the whole lab.
#
#   demo.sh setup             boot the lab VM (delegates to setup-lab.sh)
#   demo.sh verify            run the Part A push-path verify (8 checks)
#   demo.sh app [version]     build the demo app on the TARGET engine + install its unit
#   demo.sh up                start `container dev up`, backgrounded
#   demo.sh agent             (re)start the device agent on the target
#   demo.sh reload [version]  rebuild only, then wait for the hot reload to land
#   demo.sh sync              re-push + notify now, without waiting on an event
#   demo.sh status            where everything is right now
#   demo.sh logs <what>       session | agent | app   (what/where each one is)
#   demo.sh down              stop the session, agent and app; leave the VM warm
#   demo.sh reset             full wipe, back to a pre-demo state
#   demo.sh all [v1] [v2]     setup -> app -> up -> agent -> reload, end to end
#
# Every action prints a context header naming WHICH MACHINE it runs against and
# WHAT it touches, because this lab has two docker daemons - your workstation's and
# the target's - and picking the wrong one fails silently in both directions.
#
# MODE selects the topology, and it is the difference between a clear demo and a
# confusing one:
#
#   MODE=native (default)  Build on THIS workstation's engine. The target only runs
#                          the app and the agent, reached solely over ssh. Two
#                          machines, one job each, no ambiguity. This is the real
#                          topology for a Linux dev with a board, and it is what
#                          makes the pull an actual network transfer.
#
#   MODE=vm                Build on the target's own engine through the forwarded
#                          socket, emulating macOS/Windows where docker runs in a
#                          helper VM. The target then plays BOTH roles, which is
#                          what made "which docker am I talking to" ambiguous.
#                          `verify` needs this, because the VM write path is what
#                          it tests.
#
# The CLI picks the topology off DOCKER_HOST alone: is_vm_routing_active()
# (container.rs:79-89) is true iff DOCKER_HOST equals the avocado-vm socket. So
# native mode is a config choice, not a second VM.
#
# Environment:
#   MODE                  native | vm           (default: native)
#   TARGET_PLATFORM       e.g. linux/arm64      (default: empty = same arch as the
#                         build engine). Setting it switches the build to buildx,
#                         which emits NO tag event, so the sync is triggered
#                         explicitly instead of waiting on the watcher.
#   LAB_VM                1 = the target is this repo's QEMU lab VM (default: 1).
#                         Set 0 for real hardware: `setup`/`verify` then refuse
#                         rather than trying to boot or test a VM that is not there.
#   AVOCADO_CDM_LAB_WORK  generated lab state   (default: ~/repos/work/peridio-container-dev/lab)
#   AVOCADO_CLI           avocado-cli checkout  (default: derived from this script's path)
#   SSH_ALIAS             ssh alias for the target (default: avocado-vm-lab)
#   TEST_IMAGE            watched image ref     (default: my-app:dev)
#   APP_SERVICE           unit owning it        (default: app.service)
#
# Pointing this at a Raspberry Pi 5 (or any real board) is env only:
#
#   export LAB_VM=0                              # no VM to boot or verify
#   export SSH_ALIAS=pi5                         # your ssh alias for the board
#   export TARGET_PLATFORM=linux/arm64           # cross-build from an x86-64 host
#   unset AVOCADO_CONTAINER_DEV_HOST             # let the CLI detect your LAN address
#   demo.sh app v1 && demo.sh up && demo.sh agent && demo.sh reload v2
#
# Two things genuinely differ on arm64 and both are handled above rather than left
# as a surprise. The arch guard REFUSES a wrong-arch push, so an amd64 image built
# on your laptop never silently reaches an arm64 board - TARGET_PLATFORM is what
# keeps that guard satisfied. And a cross-build needs buildx, which is BuildKit and
# therefore emits no tag event, so the watcher cannot see it; the script triggers
# `container dev sync` itself in that case.
#
# Still manual for a real board: the agent binary must exist on it. Either it ships
# in the runtime as avocado-ext-container-agent-dev, or cross-compile it for
# aarch64-unknown-linux-musl (runbook B1, swapping the target triple) and copy it in.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AVOCADO_CLI="${AVOCADO_CLI:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
LAB="${AVOCADO_CDM_LAB_WORK:-$HOME/repos/work/peridio-container-dev/lab}"
SSH_ALIAS="${SSH_ALIAS:-avocado-vm-lab}"
TEST_IMAGE="${TEST_IMAGE:-my-app:dev}"
APP_SERVICE="${APP_SERVICE:-app.service}"
CONTAINER="${APP_SERVICE%.service}"
BUILD_CTX="${BUILD_CTX:-/tmp/cdm-app}"
DOCK_SOCK="${DOCK_SOCK:-$HOME/.avocado/vm/docker.sock}"
UP_LOG="${UP_LOG:-/tmp/cdm-up.log}"
MODE="${MODE:-native}"
case "$MODE" in native|vm) ;; *) echo "MODE must be native or vm, got '$MODE'" >&2; exit 1 ;; esac

B=$'\033[1m'; R=$'\033[0m'

# ---------------------------------------------------------------------------
# Context reporting. The whole point of this script: never run a docker command
# without first saying which daemon it lands on.
# ---------------------------------------------------------------------------

ctx() {
  printf '\n%s== %s ==%s\n' "$B" "$1" "$R"
  shift
  while [ $# -gt 0 ]; do printf '   %-11s %s\n' "${1%%|*}" "${1#*|}"; shift; done
}

die() { printf '\n!! %s\n' "$*" >&2; exit 1; }

# `grep -c` prints 0 AND exits 1 on no-match, so a naive `|| echo 0` prints twice.
count_in() { local n; n="$(grep -c "$1" "$2" 2>/dev/null)"; echo "${n:-0}"; }

# Which daemon does a given DOCKER_HOST answer as? The name is the daemon's own
# hostname, so `avocado-vm-lab` means the target and anything else means this box.
daemon_name() { DOCKER_HOST="$1" docker info --format '{{.Name}}' 2>/dev/null || true; }

# The two engines, as two named functions. Every docker call in this script goes
# through one of them, so no command can quietly land on the wrong machine.

# The engine that BUILDS. native: this workstation. vm: the target's, forwarded.
build_engine() {
  if [ "$MODE" = native ]; then
    env -u DOCKER_HOST docker "$@"
  else
    target_engine "$@"
  fi
}

# The engine that RUNS the app - always the target's, reached differently per mode.
# native has no forwarded socket by design, so it goes over ssh.
target_engine() {
  if [ "$MODE" = native ]; then
    ssh "$SSH_ALIAS" docker "$@"
  else
    [ -S "$DOCK_SOCK" ] || die "no forwarded target engine socket at $DOCK_SOCK - run: $0 setup"
    local name; name="$(daemon_name "unix://$DOCK_SOCK")"
    [ "$name" = "$SSH_ALIAS" ] || die "socket $DOCK_SOCK answers as '$name', expected '$SSH_ALIAS'"
    DOCKER_HOST="unix://$DOCK_SOCK" docker "$@"
  fi
}

# Human-readable description of where each engine lives, for the ctx headers.
build_engine_where() {
  if [ "$MODE" = native ]; then echo "THIS WORKSTATION's engine ($(env -u DOCKER_HOST docker info --format '{{.Name}}' 2>/dev/null || echo unreachable))"
  else echo "the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK"; fi
}
target_engine_where() {
  if [ "$MODE" = native ]; then echo "the HITL TARGET's engine ($SSH_ALIAS) over ssh"
  else echo "the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK"; fi
}

# Build the demo image. Returns 0 and sets EMITS_TAG_EVENT to 1/0 so callers know
# whether the watcher can see the rebuild or whether it must be triggered.
EMITS_TAG_EVENT=0
write_ctx() {
  local version="$1"
  mkdir -p "$BUILD_CTX"
  cat >"$BUILD_CTX/Dockerfile" <<EOF
FROM busybox:latest
RUN yes avocado | head -c 524288 > /base.bin
RUN printf '$version\\n' > /version
CMD ["sh","-c","while true; do echo \\"app \$(cat /version) base=\$(wc -c </base.bin)B running-on=\$(hostname) image=$TEST_IMAGE\\"; sleep 2; done"]
EOF
}
build_image() {
  local version="$1"
  write_ctx "$version"
  if [ -n "$TARGET_PLATFORM" ]; then
    # Cross-arch needs buildx, which is BuildKit and emits no image tag event.
    EMITS_TAG_EVENT=0
    build_engine buildx build --platform "$TARGET_PLATFORM" --load \
      -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "cross-build for $TARGET_PLATFORM failed (is buildx + binfmt set up?)"
  else
    # Same arch: the classic builder DOES emit a tag event, so the watcher fires.
    EMITS_TAG_EVENT=1
    DOCKER_BUILDKIT=0 build_engine build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "build failed"
  fi
}

# In native mode the image is built on the workstation, so the target cannot see it
# until the loop ships it. `app` therefore has to push once before the unit starts,
# or the first `docker run` on the target fails on a missing image.
seed_target_image() {
  [ "$MODE" = native ] || return 0
  session_running || return 0
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync >/dev/null 2>&1 ) || true
}

# Find the running `container dev up` session.
#
# NOT by `pgrep -f "container dev up"`: that matches ANY process whose argv happens
# to contain the phrase, including the very shell running this script if the phrase
# appears anywhere in its command line - which kills the caller. Match the process
# NAME instead (comm is `avocado`; a wrapper shell's is not) and confirm via
# /proc/<pid>/cmdline.
session_pids() {
  local pid cmd
  for pid in $(pgrep -x avocado 2>/dev/null); do
    cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)"
    case "$cmd" in *"container dev up"*) echo "$pid" ;; esac
  done
}
session_running() { [ -n "$(session_pids)" ]; }

registry_endpoints() {
  local host="${AVOCADO_CONTAINER_DEV_HOST:-10.0.2.2}"
  printf 'bulk read %s:5599 (target pulls) | write %s:5601 (host pushes, loopback-bound) | control WS %s:5600' \
    "$host" "127.0.0.1" "$host"
}

# ---------------------------------------------------------------------------

cmd_setup() {
  [ "${LAB_VM:-1}" = 1 ] || die "LAB_VM=0: the target is real hardware, there is no VM to boot"
  ctx "SETUP the lab VM" \
    "runs on|this workstation" \
    "creates|QEMU VM '$SSH_ALIAS', ssh 127.0.0.1:2222, forwarded engine socket $DOCK_SOCK" \
    "note|with no engine.qcow2 the first boot runs cloud-init (installs docker.io): minutes, needs network"
  AVOCADO_CDM_LAB_WORK="$LAB" AVOCADO_CLI="$AVOCADO_CLI" bash "$SCRIPT_DIR/setup-lab.sh" || die "setup-lab.sh failed"
}

cmd_verify() {
  [ "${LAB_VM:-1}" = 1 ] || die "LAB_VM=0: verify-vm-write-path.sh tests the QEMU VM write path only"
  [ "$MODE" = vm ] || die "verify tests the VM write path - re-run as: MODE=vm $0 verify"
  ctx "VERIFY the authenticated push path" \
    "runs on|this workstation" \
    "reaches|$SSH_ALIAS over ssh, and its engine over $DOCK_SOCK" \
    "note|starts and tears down its OWN session, and rotates the target's bootstrap token"
  # shellcheck source=/dev/null
  source "$LAB/env.sh"
  ( cd "$AVOCADO_CLI" && ./docs/container-dev/verify-vm-write-path.sh )
}

cmd_app() {
  local version="${1:-v1}"
  ctx "BUILD the demo app" \
    "mode|$MODE" \
    "builds on|$(build_engine_where)" \
    "image|$TEST_IMAGE   version=$version${TARGET_PLATFORM:+   platform=$TARGET_PLATFORM}" \
    "builder|$([ -n "$TARGET_PLATFORM" ] && echo "buildx (cross-arch; emits no tag event)" || echo "classic, DOCKER_BUILDKIT=0 (emits the tag event the watcher needs)")" \
    "runs on|$(target_engine_where)"

  build_image "$version"
  seed_target_image

  ctx "INSTALL the owning service" \
    "installs on|the HITL TARGET ($SSH_ALIAS), over ssh" \
    "unit|/etc/systemd/system/$APP_SERVICE  ->  docker run --name $CONTAINER $TEST_IMAGE" \
    "why|the agent restarts this UNIT; an engine 'restart' would re-run the pinned image ID and silently keep the old code"

  # shellcheck disable=SC2087  # local expansion is intended: bake the refs in
  ssh "$SSH_ALIAS" "cat > /etc/systemd/system/$APP_SERVICE" <<EOF
[Unit]
Description=Container Dev Mode demo app
Requires=docker.service
After=docker.service

[Service]
ExecStartPre=-/usr/bin/docker rm -f $CONTAINER
# --hostname %H: systemd expands %H to THIS machine's hostname, so the app's own
# log line names the machine it runs on. Without it a container reports its own
# container ID, which tells you nothing about which side of the loop you are on.
ExecStart=/usr/bin/docker run --rm --name $CONTAINER --hostname %H $TEST_IMAGE
ExecStop=-/usr/bin/docker stop $CONTAINER
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
  ssh "$SSH_ALIAS" "systemctl daemon-reload && systemctl enable $APP_SERVICE >/dev/null 2>&1; systemctl restart $APP_SERVICE" \
    || die "could not start $APP_SERVICE on $SSH_ALIAS"

  sleep 4
  local line; line="$(target_engine logs --tail 1 "$CONTAINER" 2>&1)"
  ctx "APP is up" "reading|$(target_engine_where)" "says|$line"
  case "$line" in
    *"$version"*) printf '   %-11s %s\n' "result" "baseline $version confirmed on the target" ;;
    *) die "app is not reporting '$version' - ssh $SSH_ALIAS 'journalctl -u $APP_SERVICE -n 20'" ;;
  esac
}

cmd_up() {
  session_running && die "a session is already running - $0 down first"
  ctx "START the dev session" \
    "runs on|this workstation" \
    "serves|$(registry_endpoints)" \
    "store|$HOME/.avocado/container-dev/<runtime>/registry/" \
    "reaches|$SSH_ALIAS once over ssh to bootstrap it, then never again" \
    "log|$UP_LOG (every push shows up here)"
  # The CLI reads ./avocado.yaml from the cwd, not $AVOCADO_CONFIG.
  # shellcheck source=/dev/null
  [ -f "$LAB/env.sh" ] && source "$LAB/env.sh"
  if [ "$MODE" = native ]; then
    # env.sh points DOCKER_HOST at the VM socket, and is_vm_routing_active() keys
    # on exactly that. Unset it or the CLI takes the vm path and builds/pushes
    # through the target's engine - the topology native mode exists to avoid.
    unset DOCKER_HOST
  fi
  # setsid + </dev/null fully detaches: without closing stdin the background
  # session keeps the caller's pipeline open and `demo.sh up` never returns.
  ( cd "$SCRIPT_DIR" && setsid nohup "$AVOCADO_BIN" container dev up >"$UP_LOG" 2>&1 </dev/null & )
  sleep 15
  grep -qE "bulk listener" "$UP_LOG" || { tail -5 "$UP_LOG"; die "session did not come up - see $UP_LOG"; }
  sed -e 's/^/   /' <(tail -2 "$UP_LOG")
}

cmd_agent() {
  ctx "START the device agent" \
    "runs on|the HITL TARGET ($SSH_ALIAS), over ssh" \
    "pulls via|its own loopback proxy 127.0.0.1:15151 -> the host's bulk listener" \
    "restarts|$APP_SERVICE (AVOCADO_CONTAINER_DEV_SERVICE)" \
    "note|stopped first: systemd-run refuses silently if active, leaving a stale-CA agent"
  ssh "$SSH_ALIAS" 'systemctl stop cdm-agent 2>/dev/null; true'
  ssh "$SSH_ALIAS" "systemd-run --unit=cdm-agent --collect \
    --setenv=AVOCADO_CONTAINER_DEV_SERVICE=$APP_SERVICE \
    /usr/local/bin/avocado-container-agent-dev" >/dev/null 2>&1 \
    || die "could not start the agent - is /usr/local/bin/avocado-container-agent-dev present? (runbook B1)"
  sleep 5
  ssh "$SSH_ALIAS" 'journalctl -u cdm-agent --no-pager -n 3 -o cat' 2>/dev/null | sed -e 's/^/   /'
}

cmd_reload() {
  local version="${1:-v2-RELOADED}"
  local before; before="$(target_engine logs --tail 1 "$CONTAINER" 2>&1)"
  ctx "RELOAD: rebuild only, let the loop do the rest" \
    "mode|$MODE" \
    "builds on|$(build_engine_where)" \
    "version|$version${TARGET_PLATFORM:+   platform=$TARGET_PLATFORM}" \
    "pushes to|the host's write listener 127.0.0.1:5601, tagged 10.0.2.2:5601/${TEST_IMAGE%%:*}" \
    "then|control WS notifies the target, which pulls by digest and restarts $APP_SERVICE" \
    "note|the unit is NOT touched here, so only the watcher path can move the container" \
    "before|$before"

  build_image "$version"
  if [ "$EMITS_TAG_EVENT" = 0 ]; then
    printf '   %-11s %s\n' "trigger" "buildx emits no tag event, so triggering the sync explicitly"
    ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync >/dev/null 2>&1 ) \
      || die "container dev sync failed - is a session up? ($0 up)"
  fi

  printf '   %-11s ' "waiting"
  local line=""
  for _ in $(seq 1 30); do
    sleep 2; printf '.'
    line="$(target_engine logs --tail 1 "$CONTAINER" 2>&1)"
    case "$line" in *"$version"*) break ;; esac
  done
  printf '\n'

  ctx "RESULT" \
    "reading|$(target_engine_where)" \
    "after|$line" \
    "pushes|$(count_in 'The push refers' "$UP_LOG") in $UP_LOG, $(count_in 'no basic auth credentials' "$UP_LOG") auth failures"
  case "$line" in
    *"$version"*) printf '   %-11s %s\n' "result" "hot reload landed: the watcher moved the target to $version" ;;
    *) die "no reload after 60s - check: $0 logs session ; $0 logs agent" ;;
  esac
}

cmd_sync() {
  ctx "SYNC: re-push and notify, without waiting on an event" \
    "runs on|this workstation" \
    "pushes|whatever the BUILD engine currently holds under $TEST_IMAGE" \
    "caveat|if your image went to the other engine, this pushes the stale one and reports success"
  # shellcheck source=/dev/null
  [ -f "$LAB/env.sh" ] && source "$LAB/env.sh"
  [ "$MODE" = native ] && unset DOCKER_HOST
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync ) || die "sync failed - is a session up?"
}

cmd_status() {
  local host_daemon target_daemon
  host_daemon="$(env -u DOCKER_HOST docker info --format '{{.Name}}' 2>/dev/null || echo '(unreachable)')"
  target_daemon="$(daemon_name "unix://$DOCK_SOCK")"; : "${target_daemon:=(unreachable)}"

  ctx "WHERE THINGS ARE" \
    "workstation|engine '$host_daemon'  <- your builds land here if DOCKER_HOST is unset" \
    "HITL target|engine '$target_daemon' via $DOCK_SOCK, shell via 'ssh $SSH_ALIAS'" \
    "registry|$(registry_endpoints)" \
    "store|$HOME/.avocado/container-dev/"

  local up_pid; up_pid="$(session_pids | head -1)"
  ctx "SESSION (workstation)" \
    "up|$([ -n "$up_pid" ] && echo "running (pid $up_pid)" || echo 'not running')" \
    "log|$UP_LOG" \
    "pushes|$(count_in 'The push refers' "$UP_LOG"), auth failures $(count_in 'no basic auth credentials' "$UP_LOG")"

  if [ "$target_daemon" = "$SSH_ALIAS" ]; then
    ctx "TARGET ($SSH_ALIAS)" \
      "agent|$(ssh "$SSH_ALIAS" 'systemctl is-active cdm-agent' 2>/dev/null || echo unknown)" \
      "service|$APP_SERVICE $(ssh "$SSH_ALIAS" "systemctl is-active $APP_SERVICE" 2>/dev/null || echo unknown)" \
      "app says|$(target_engine logs --tail 1 "$CONTAINER" 2>&1 | tail -1)" \
      "running|$(ssh "$SSH_ALIAS" 'cat /var/lib/avocado/container-dev/active-image.json 2>/dev/null | tr -d "\n " ' 2>/dev/null || echo '(no pointer yet)')"
  fi
}

cmd_logs() {
  case "${1:-}" in
    session)
      ctx "SESSION LOG" "from|this workstation" "file|$UP_LOG"
      tail -30 "$UP_LOG" ;;
    agent)
      ctx "AGENT LOG" "from|the HITL TARGET ($SSH_ALIAS)" "source|journalctl -u cdm-agent"
      ssh "$SSH_ALIAS" 'journalctl -u cdm-agent --no-pager -n 30 -o cat' ;;
    app)
      ctx "APP LOG" "from|the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" "source|docker logs $CONTAINER"
      target_engine logs --tail 30 "$CONTAINER" ;;
    *) die "usage: $0 logs session|agent|app" ;;
  esac
}

cmd_down() {
  ctx "STOP the demo" "affects|this workstation (session) and the target (agent, app)" "keeps|the VM running and warm"
  # shellcheck source=/dev/null
  [ -f "$LAB/env.sh" ] && source "$LAB/env.sh"
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev down 2>/dev/null | sed -e 's/^/   /' ) || true
  session_pids | while read -r p; do kill "$p" 2>/dev/null; done
  ssh "$SSH_ALIAS" "systemctl stop cdm-agent $APP_SERVICE 2>/dev/null; docker stop $CONTAINER 2>/dev/null; docker rm $CONTAINER 2>/dev/null; true" >/dev/null 2>&1
  printf '   %-11s %s\n' "done" "session, agent and app stopped"
}

cmd_reset() {
  ctx "RESET to a pre-demo state" \
    "deletes|the guest disk (engine.qcow2) and every generated seed artifact" \
    "deletes|the host registry store and the demo build context" \
    "keeps|debian12.qcow2 and id_lab* - inputs, not state"
  session_pids | while read -r p; do kill "$p" 2>/dev/null; done
  sleep 2
  pkill -f "$DOCK_SOCK:" 2>/dev/null; rm -f "$DOCK_SOCK"
  if [ -f "$LAB/qemu.pid" ]; then
    local qp; qp="$(cat "$LAB/qemu.pid")"
    kill "$qp" 2>/dev/null
    for _ in $(seq 1 10); do kill -0 "$qp" 2>/dev/null || break; sleep 1; done
    kill -9 "$qp" 2>/dev/null
    rm -f "$LAB/qemu.pid"
  fi
  rm -f "$LAB/engine.qcow2" "$LAB/seed.iso" "$LAB/user-data" "$LAB/meta-data" "$LAB/console.log" "$LAB/curl.log"
  rm -rf "$HOME/.avocado/container-dev" "$BUILD_CTX"
  printf '   %-11s %s\n' "done" "start again with: $0 all"
}

cmd_all() {
  local v1="${1:-v1}" v2="${2:-v2-RELOADED}"
  [ "${LAB_VM:-1}" = 1 ] && cmd_setup
  cmd_app "$v1"
  cmd_up
  cmd_agent
  cmd_reload "$v2"
  cmd_status
}

case "${1:-}" in
  setup)  shift; cmd_setup "$@" ;;
  verify) shift; cmd_verify "$@" ;;
  app)    shift; cmd_app "$@" ;;
  up)     shift; cmd_up "$@" ;;
  agent)  shift; cmd_agent "$@" ;;
  reload) shift; cmd_reload "$@" ;;
  sync)   shift; cmd_sync "$@" ;;
  status) shift; cmd_status "$@" ;;
  logs)   shift; cmd_logs "$@" ;;
  down)   shift; cmd_down "$@" ;;
  reset)  shift; cmd_reset "$@" ;;
  all)    shift; cmd_all "$@" ;;
  ""|-h|--help|help)
    # Print the header comment block: from line 3 until the first non-comment line.
    awk 'NR>=3 && /^#/ { sub(/^# ?/, ""); print; next } NR>=3 { exit }' "${BASH_SOURCE[0]}"
    ;;
  *) die "unknown command '${1}' - run '$0 help'" ;;
esac
