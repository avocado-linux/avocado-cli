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
# Environment:
#   AVOCADO_CDM_LAB_WORK  generated lab state   (default: ~/repos/work/peridio-container-dev/lab)
#   AVOCADO_CLI           avocado-cli checkout  (default: derived from this script's path)
#   SSH_ALIAS             ssh alias for the VM  (default: avocado-vm-lab)
#   TEST_IMAGE            watched image ref     (default: my-app:dev)
#   APP_SERVICE           unit owning it        (default: app.service)

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

# Every build/push/logs call goes through here so it cannot silently hit the wrong
# engine: it resolves the target's daemon and refuses if the socket answers wrong.
target_docker() {
  [ -S "$DOCK_SOCK" ] || die "no forwarded target engine socket at $DOCK_SOCK - run: $0 setup"
  local name; name="$(daemon_name "unix://$DOCK_SOCK")"
  [ "$name" = "$SSH_ALIAS" ] || die "socket $DOCK_SOCK answers as '$name', expected '$SSH_ALIAS'"
  DOCKER_HOST="unix://$DOCK_SOCK" docker "$@"
}

registry_endpoints() {
  local host="${AVOCADO_CONTAINER_DEV_HOST:-10.0.2.2}"
  printf 'bulk read %s:5599 (target pulls) | write %s:5601 (host pushes, loopback-bound) | control WS %s:5600' \
    "$host" "127.0.0.1" "$host"
}

# ---------------------------------------------------------------------------

cmd_setup() {
  ctx "SETUP the lab VM" \
    "runs on|this workstation" \
    "creates|QEMU VM '$SSH_ALIAS', ssh 127.0.0.1:2222, forwarded engine socket $DOCK_SOCK" \
    "note|with no engine.qcow2 the first boot runs cloud-init (installs docker.io): minutes, needs network"
  AVOCADO_CDM_LAB_WORK="$LAB" AVOCADO_CLI="$AVOCADO_CLI" bash "$SCRIPT_DIR/setup-lab.sh" || die "setup-lab.sh failed"
}

cmd_verify() {
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
    "builds on|the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" \
    "image|$TEST_IMAGE   version=$version" \
    "builder|classic (DOCKER_BUILDKIT=0) so the engine emits a tag event the watcher can see" \
    "not on|this workstation's engine - an image built there would never reach the target"

  mkdir -p "$BUILD_CTX"
  # The app prints its own hostname, so a log line proves WHICH machine it runs on.
  cat >"$BUILD_CTX/Dockerfile" <<EOF
FROM busybox:latest
RUN yes avocado | head -c 524288 > /base.bin
RUN printf '$version\\n' > /version
CMD ["sh","-c","while true; do echo \\"app \$(cat /version) base=\$(wc -c </base.bin)B running-on=\$(hostname) image=$TEST_IMAGE\\"; sleep 2; done"]
EOF
  DOCKER_BUILDKIT=0 target_docker build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "build failed"

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
  local line; line="$(target_docker logs --tail 1 "$CONTAINER" 2>&1)"
  ctx "APP is up" "reading|the TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" "says|$line"
  case "$line" in
    *"$version"*) printf '   %-11s %s\n' "result" "baseline $version confirmed on the target" ;;
    *) die "app is not reporting '$version' - ssh $SSH_ALIAS 'journalctl -u $APP_SERVICE -n 20'" ;;
  esac
}

cmd_up() {
  pgrep -f "[c]ontainer dev up" >/dev/null && die "a session is already running - $0 down first"
  ctx "START the dev session" \
    "runs on|this workstation" \
    "serves|$(registry_endpoints)" \
    "store|$HOME/.avocado/container-dev/<runtime>/registry/" \
    "reaches|$SSH_ALIAS once over ssh to bootstrap it, then never again" \
    "log|$UP_LOG (every push shows up here)"
  # The CLI reads ./avocado.yaml from the cwd, not $AVOCADO_CONFIG.
  # shellcheck source=/dev/null
  source "$LAB/env.sh"
  ( cd "$SCRIPT_DIR" && nohup "$AVOCADO_BIN" container dev up >"$UP_LOG" 2>&1 & )
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
  local before; before="$(target_docker logs --tail 1 "$CONTAINER" 2>&1)"
  ctx "RELOAD: rebuild only, let the loop do the rest" \
    "builds on|the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" \
    "version|$version" \
    "pushes to|the host's write listener 127.0.0.1:5601, tagged 10.0.2.2:5601/${TEST_IMAGE%%:*}" \
    "then|control WS notifies the target, which pulls by digest and restarts $APP_SERVICE" \
    "note|the unit is NOT touched here, so only the watcher path can move the container" \
    "before|$before"

  mkdir -p "$BUILD_CTX"
  cat >"$BUILD_CTX/Dockerfile" <<EOF
FROM busybox:latest
RUN yes avocado | head -c 524288 > /base.bin
RUN printf '$version\\n' > /version
CMD ["sh","-c","while true; do echo \\"app \$(cat /version) base=\$(wc -c </base.bin)B running-on=\$(hostname) image=$TEST_IMAGE\\"; sleep 2; done"]
EOF
  DOCKER_BUILDKIT=0 target_docker build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "build failed"

  printf '   %-11s ' "waiting"
  local line=""
  for _ in $(seq 1 30); do
    sleep 2; printf '.'
    line="$(target_docker logs --tail 1 "$CONTAINER" 2>&1)"
    case "$line" in *"$version"*) break ;; esac
  done
  printf '\n'

  ctx "RESULT" \
    "reading|the TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" \
    "after|$line" \
    "pushes|$(count_in 'The push refers' "$UP_LOG") in $UP_LOG, $(count_in 'no basic auth credentials' "$UP_LOG") auth failures"
  case "$line" in
    *"$version"*) printf '   %-11s %s\n' "result" "hot reload landed: the watcher moved the target to $version" ;;
    *) die "no reload after 60s - check: $0 logs session ; $0 logs agent" ;;
  esac
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

  local up_n; up_n="$(pgrep -cf '[c]ontainer dev up' || true)"
  ctx "SESSION (workstation)" \
    "up|$([ "${up_n:-0}" -gt 0 ] && echo "running (pid $(pgrep -f '[c]ontainer dev up' | head -1))" || echo 'not running')" \
    "log|$UP_LOG" \
    "pushes|$(count_in 'The push refers' "$UP_LOG"), auth failures $(count_in 'no basic auth credentials' "$UP_LOG")"

  if [ "$target_daemon" = "$SSH_ALIAS" ]; then
    ctx "TARGET ($SSH_ALIAS)" \
      "agent|$(ssh "$SSH_ALIAS" 'systemctl is-active cdm-agent' 2>/dev/null || echo unknown)" \
      "service|$APP_SERVICE $(ssh "$SSH_ALIAS" "systemctl is-active $APP_SERVICE" 2>/dev/null || echo unknown)" \
      "app says|$(target_docker logs --tail 1 "$CONTAINER" 2>&1 | tail -1)" \
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
      target_docker logs --tail 30 "$CONTAINER" ;;
    *) die "usage: $0 logs session|agent|app" ;;
  esac
}

cmd_down() {
  ctx "STOP the demo" "affects|this workstation (session) and the target (agent, app)" "keeps|the VM running and warm"
  # shellcheck source=/dev/null
  [ -f "$LAB/env.sh" ] && source "$LAB/env.sh"
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev down 2>/dev/null | sed -e 's/^/   /' ) || true
  pgrep -f "[c]ontainer dev up" | while read -r p; do kill "$p" 2>/dev/null; done
  ssh "$SSH_ALIAS" "systemctl stop cdm-agent $APP_SERVICE 2>/dev/null; docker stop $CONTAINER 2>/dev/null; docker rm $CONTAINER 2>/dev/null; true" >/dev/null 2>&1
  printf '   %-11s %s\n' "done" "session, agent and app stopped"
}

cmd_reset() {
  ctx "RESET to a pre-demo state" \
    "deletes|the guest disk (engine.qcow2) and every generated seed artifact" \
    "deletes|the host registry store and the demo build context" \
    "keeps|debian12.qcow2 and id_lab* - inputs, not state"
  pgrep -f "[c]ontainer dev up" | while read -r p; do kill "$p" 2>/dev/null; done
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
  cmd_setup
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
