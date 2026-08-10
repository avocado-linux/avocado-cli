#!/usr/bin/env bash
#
# Container Dev Mode demo driver - one entry point for the whole lab.
#
#   demo.sh setup             boot the lab VM (delegates to setup-lab.sh)
#   demo.sh verify            run the Part A push-path verify (8 checks)
#   demo.sh app [version]     build the demo app on the TARGET engine + install its unit
#   demo.sh seed              ship what the host holds; reads the version off the image
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
#   SSH_ALIAS             ssh alias for the target (default: avocado-hitl)
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
# The agent needs to be ON the board before any of this works, and the supported
# way is the runtime: include avocado-ext-container-agent-dev and it ships as a
# real unit, which is what AGENT_UNIT below expects. Verified on an FRDM-IMX93 -
# `demo.sh agent` restarts that unit and nothing is hand-placed.
#
# Cross-compiling the binary for aarch64-unknown-linux-musl (runbook B1, swapping
# the target triple) and copying it in still works, but it is the fallback for a
# board whose runtime predates the extension, not the normal path.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AVOCADO_CLI="${AVOCADO_CLI:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
# Generated lab state, matching setup-lab.sh's own default. It lives outside any
# repo checkout because the target's disk image is ~1 GB.
LAB="${AVOCADO_CDM_LAB_WORK:-$HOME/.cache/avocado-cdm-lab}"
# Where the bootable target artifacts and its qemu pidfile live.
VMDIR="${VMDIR:-$LAB/hitl-vm}"
SSH_ALIAS="${SSH_ALIAS:-avocado-hitl}"
# The agent ships in the runtime as a real unit (avocado-ext-container-agent-dev),
# so there is nothing to cross-compile and copy any more. It used to be started as
# a transient `cdm-agent` via systemd-run against a hand-placed binary.
AGENT_UNIT="${AGENT_UNIT:-container-agent-dev}"
# The target's docker daemon reports its own hostname, which is the Avocado image's
# hostname (avocado-<target>) and NOT the ssh alias. setup-lab.sh exports the real
# value; fall back to asking the target so a bare run still works.
TARGET_HOSTNAME="${TARGET_HOSTNAME:-}"
# The lab alias uses UserKnownHostsFile=/dev/null, so ssh prints "Permanently
# added ..." on every connection. That noise ends up inside captured command output
# and reads as if it came from the app, so quiet it at the source.
SSH_Q=(ssh -o LogLevel=ERROR)
TEST_IMAGE="${TEST_IMAGE:-my-app:dev}"
APP_SERVICE="${APP_SERVICE:-app.service}"
CONTAINER="${APP_SERVICE%.service}"
BUILD_CTX="${BUILD_CTX:-/tmp/cdm-app}"
DOCK_SOCK="${DOCK_SOCK:-$HOME/.avocado/vm/docker.sock}"
UP_LOG="${UP_LOG:-/tmp/cdm-up.log}"
MODE="${MODE:-native}"
TARGET_PLATFORM="${TARGET_PLATFORM:-}"
LAB_VM="${LAB_VM:-1}"
case "$MODE" in native|vm) ;; *) echo "MODE must be native or vm, got '$MODE'" >&2; exit 1 ;; esac

B=$'\033[1m'; R=$'\033[0m'

# ---------------------------------------------------------------------------
# Context reporting. The whole point of this script: never run a docker command
# without first saying which daemon it lands on.
# ---------------------------------------------------------------------------

ctx() {
  # Silent in presentation mode: these blocks are the debugging instrument, and
  # every one of them names a machine, a port or a path the viewer must not be
  # asked to care about.
  [ "${PRESENT:-0}" = 1 ] && return 0
  printf '\n%s== %s ==%s\n' "$B" "$1" "$R"
  shift
  while [ $# -gt 0 ]; do printf '   %-11s %s\n' "${1%%|*}" "${1#*|}"; shift; done
}

die() { printf '\n!! %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Presentation mode (PRESENT=1).
#
# Two disjoint output families rather than one mode-aware renderer, because the
# two views need different DATA, not different formatting: the debug view wants
# the ephemeral write port, the presentation view wants bytes on the wire. A
# shared model would carry the union and serve neither. They are kept honest by
# living side by side in the same cmd_* function, not by sharing plumbing.
#
# WIDTH IS 64 COLUMNS, HARD. The recording composites the logo, timer and
# caption frameless into the bottom-right of the frame - measured at terminal
# rows 35-40, columns 66-93 - and content scrolls up through those rows, so any
# line wider than 64 eventually collides with the branding. The failure is
# invisible until you watch the finished MP4, so p() checks it.
# ---------------------------------------------------------------------------

PRESENT="${PRESENT:-0}"
_PW=64

present() { [ "$PRESENT" = 1 ]; }

# A horizontal rule padded to _PW, dim.
prule() {
  present || return 0
  local s="${1:-}" n
  n=$(( _PW - ${#s} )); [ "$n" -lt 0 ] && n=0
  printf '\n\033[2m%s' "$s"
  [ "$n" -gt 0 ] && printf '─%.0s' $(seq 1 "$n")
  printf '\033[0m\n'
}

# A plain line. Warns to stderr if it would collide with the branding.
p() {
  present || return 0
  local s="${1:-}"
  [ "${#s}" -gt "$_PW" ] && printf 'warn: presentation line is %d cols (max %d): %s\n' "${#s}" "$_PW" "$s" >&2
  printf '%s\n' "$s"
}

pkv()  { present || return 0; printf '  %-18s %s\n' "$1" "$2"; }
# A payoff number in brand green - the only colour in presentation mode. The
# trailing note is optional, and its space is dropped when absent so the line
# carries no trailing whitespace.
pnum() {
  present || return 0
  if [ -n "${3:-}" ]; then printf '  %-18s \033[32m%s\033[0m  %s\n' "$1" "$2" "$3"
  else                     printf '  %-18s \033[32m%s\033[0m\n'     "$1" "$2"; fi
}
pstep(){ present || return 0; prule "──[ $1 ]── $2 "; }
pbeat(){ present || return 0; sleep "${1:-2}"; }
# Multi-line block from a heredoc. Consumes stdin even when disabled, so an
# unread heredoc cannot confuse a later reader.
pblock() { present || { cat >/dev/null; return 0; }; cat; }

# Bytes as something a viewer can read at a glance.
human() {
  local b="${1:-0}"
  if   [ "$b" -ge 1048576 ]; then awk -v b="$b" 'BEGIN{printf "%.1f MB", b/1048576}'
  elif [ "$b" -ge 1024 ];    then awk -v b="$b" 'BEGIN{printf "%.1f KB", b/1024}'
  else printf '%d B' "$b"; fi
}

# Total bytes in the session's blob store. The store IS the wire: the registry
# writes each pushed blob once and has_blob() short-circuits a re-push, so the
# growth across a delivery is exactly what crossed to the device.
STORE_ROOT="$HOME/.avocado/container-dev"
store_bytes() { du -sb "$STORE_ROOT" 2>/dev/null | awk '{print $1+0}'; }

# $LAB/env.sh is state written by setup-lab.sh and it describes THE LAB VM: its
# ssh alias, its SLIRP host alias, its forwarded engine socket. Sourcing it uses
# `export`, so it overrides whatever the caller set - which is silently wrong on
# real hardware. A leftover env.sh from an earlier lab run sent an `up` aimed at
# a board to 127.0.0.1:2222 instead, and the only symptom was ssh refusing a
# connection to a VM that was not running.
#
# LAB_VM=0 means there is no lab VM, so there is nothing in that file worth
# having. Skip it rather than letting it win.
source_lab_env() {
  [ "$LAB_VM" = 1 ] || return 0
  [ -f "$LAB/env.sh" ] || return 0
  # shellcheck source=/dev/null
  source "$LAB/env.sh"
}

# `grep -c` prints 0 AND exits 1 on no-match, so a naive `|| echo 0` prints twice.
count_in() { local n; n="$(grep -c "$1" "$2" 2>/dev/null)"; echo "${n:-0}"; }

# Which daemon does a given DOCKER_HOST answer as? The name is the daemon's own
# hostname, so matching it against the target's hostname says which box answered.
daemon_name() { DOCKER_HOST="$1" docker info --format '{{.Name}}' 2>/dev/null || true; }

# The target's own hostname, resolved once and cached. NOT the ssh alias: an
# Avocado OS image is named avocado-<target>, so comparing a daemon's reported
# name against the alias would never match and every socket check would fail.
target_hostname() {
  if [ -z "$TARGET_HOSTNAME" ]; then
    TARGET_HOSTNAME="$("${SSH_Q[@]}" "$SSH_ALIAS" hostname 2>/dev/null || true)"
  fi
  echo "$TARGET_HOSTNAME"
}

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
    "${SSH_Q[@]}" "$SSH_ALIAS" docker "$@"
  else
    [ -S "$DOCK_SOCK" ] || die "no forwarded target engine socket at $DOCK_SOCK - run: $0 setup"
    local name want; name="$(daemon_name "unix://$DOCK_SOCK")"; want="$(target_hostname)"
    [ "$name" = "$want" ] || die "socket $DOCK_SOCK answers as '$name', expected the target '$want'"
    DOCKER_HOST="unix://$DOCK_SOCK" docker "$@"
  fi
}

# Human-readable description of where each engine lives, for the ctx headers.
# Which builder build_image will pick, and why - for the context header.
build_desc() {
  if [ -n "$TARGET_PLATFORM" ]; then echo "buildx cross-arch -> no tag event, sync triggered explicitly"; return; fi
  local srv major
  srv="$(build_engine version --format '{{.Server.Version}}' 2>/dev/null || echo 0)"
  major="${srv%%.*}"; case "$major" in ''|*[!0-9]*) major=0 ;; esac
  if [ "$major" -ge 23 ]; then echo "BuildKit (docker $srv emits the tag event the watcher needs)"
  else echo "classic, DOCKER_BUILDKIT=0 (docker $srv emits no event for BuildKit builds)"; fi
}

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
# Same version as a label so \`seed\` can read it with \`image inspect\`, which does
# not execute the image. A cross-arch build (TARGET_PLATFORM) is not runnable on
# the build host unless binfmt is registered there, so reading /version by running
# the image would break the very flow this lab advertises.
LABEL org.avocado.demo.version="$version"
CMD ["sh","-c","while true; do echo \\"app \$(cat /version) base=\$(wc -c </base.bin)B running-on=\$(hostname) image=$TEST_IMAGE\\"; sleep 2; done"]
EOF
}
build_image() {
  local version="$1"
  write_ctx "$version"
  # Whether the build engine emits an `image tag` event for a BuildKit build is a
  # DAEMON-VERSION question, not a BuildKit one. Measured: docker 20.10.24 emits
  # nothing (so the watcher is blind and the classic builder is required); docker
  # 29.6.2 emits `image tag` normally. Gate on the major version rather than always
  # forcing the classic builder, which Docker has deprecated.
  local srv major
  srv="$(build_engine version --format '{{.Server.Version}}' 2>/dev/null || echo 0)"
  major="${srv%%.*}"; case "$major" in ''|*[!0-9]*) major=0 ;; esac

  if [ -n "$TARGET_PLATFORM" ]; then
    # Cross-arch needs buildx, which is BuildKit and emits no image tag event.
    EMITS_TAG_EVENT=0
    build_engine buildx build --platform "$TARGET_PLATFORM" --load \
      -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "cross-build for $TARGET_PLATFORM failed (is buildx + binfmt set up?)"
  elif [ "$major" -ge 23 ]; then
    # Modern daemon: BuildKit is fine, and the watcher sees the tag event.
    EMITS_TAG_EVENT=1
    build_engine build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "build failed"
  else
    # Old daemon (<23): BuildKit emits no image event at all, so fall back to the
    # classic builder, which does.
    EMITS_TAG_EVENT=1
    DOCKER_BUILDKIT=0 build_engine build -q -t "$TEST_IMAGE" "$BUILD_CTX" >/dev/null || die "build failed"
  fi
}

# Is the watched image present on the TARGET's engine?
# Does the TARGET hold exactly the image the HOST currently has under the watched
# tag? Presence of the tag is NOT the question, and testing it was a real bug: a
# tag left behind by an earlier run points at a different image, passes a presence
# check, and makes the demo assert a version that was never delivered.
#
# Comparing image IDs is sound here because an ID is the digest of the image
# config, which a push/pull round trip preserves - verified by finding the
# target's running image present on the host under the previous session's
# registry tags, same ID.
host_image_id() { build_engine image inspect "$TEST_IMAGE" --format '{{.Id}}' 2>/dev/null; }
target_image_id() { target_engine image inspect "$TEST_IMAGE" --format '{{.Id}}' 2>/dev/null; }

# One line of the app's own output, or a plain statement that there is nothing to
# read yet. `docker logs` on a container that does not exist writes an error to
# stderr, and capturing that with 2>&1 put "Error response from daemon: No such
# container: app" in the reload readout - which reads as a fault when it is just
# the state before the first delivery, and is exactly what a demo should not show.
app_line() {
  local out
  out="$(target_engine logs --tail 1 "$CONTAINER" 2>/dev/null)" || { printf 'not running yet'; return 0; }
  if [ -n "$out" ]; then printf '%s' "$out"; else printf 'running, no output yet'; fi
}
# Whether the target holds the image the host currently has under $TEST_IMAGE.
#
# Compared by the demo VERSION LABEL rather than by image ID, because the two
# sides do not report the same kind of ID. A daemon using the containerd image
# store reports `.Id` as the MANIFEST digest; a locally built image reports the
# CONFIG digest. Measured between this workstation and an FRDM imx93 running the
# docker extension: host b9a5390906 (config) against target 7c9b269170 (manifest,
# confirmed by the target's own RepoDigests). Those never match, so an ID
# comparison failed `seed` on every run whether or not delivery had worked.
#
# The label is content, so it survives the digest difference - and it still
# rejects the stale-tag case this check exists for, because a tag left by an
# earlier run carries that run's version.
# The demo version label as the TARGET reports it, or empty.
#
# The format string must contain NO SPACES. target_engine runs
# `ssh <alias> docker ...`, and ssh joins its arguments into one string that the
# REMOTE shell re-splits on whitespace, so any space inside `{{...}}` arrives as
# separate arguments. Measured against the board:
#
#   --format '{{json .Config.Labels}}'  -> template parsing error: unclosed action
#   --format '{{.Config.Labels}}'       -> map[org.avocado.demo.version:v3]
#
# That also rules out `{{index .Config.Labels "..."}}`, which is why this parses
# the map rendering instead of asking the template for one key. The same
# constraint applies to every format passed through target_engine.
target_demo_version() {
  target_engine image inspect --format '{{.Config.Labels}}' "$TEST_IMAGE" 2>/dev/null \
    | sed -n 's/.*org\.avocado\.demo\.version:\([^] ]*\).*/\1/p' | tr -d '\r\n'
}

target_has_host_image() {
  local want got
  want="$(build_engine image inspect --format '{{index .Config.Labels "org.avocado.demo.version"}}' "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')"
  got="$(target_demo_version)"
  case "$want" in ''|'<no value>') return 1 ;; esac
  [ "$want" = "$got" ]
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

# The write listener's actual host:port, read from what the session reported.
#
# It is NOT the configured port: the session binds an EPHEMERAL loopback port
# (37633 and 41753 across two observed runs). It is also not the guest-facing
# 10.0.2.2 - the push goes to 127.0.0.1, which is what the pushed tag shows
# (`The push refers to repository [127.0.0.1:41753/my-app]`). Both were previously
# hardcoded as 10.0.2.2:5601, which described a path that never existed. This
# matters beyond cosmetics: the push credential is keyed on the tagged host:port
# byte-for-byte, so a reader debugging an auth failure needs the real pair.
write_endpoint() {
  local wport
  wport="$(sed -n 's/.*write listener loopback-only on 127\.0\.0\.1:\([0-9]\+\).*/\1/p' \
    "$UP_LOG" 2>/dev/null | tail -1)"
  if [ -n "$wport" ]; then echo "127.0.0.1:$wport"
  else echo "127.0.0.1:${AVOCADO_CONTAINER_DEV_WRITE_PORT:-5601} (configured; no session has bound one yet)"; fi
}

registry_endpoints() {
  # 10.0.2.2 is the SLIRP alias every QEMU guest sees the host as, so it is the
  # right default for the lab VM and wrong everywhere else. On real hardware the
  # host address is whatever the CLI auto-detects as reachable from the board,
  # which is not knowable before a session binds - printing the SLIRP literal
  # there advertises an address nothing is listening on.
  local host
  if [ -n "${AVOCADO_CONTAINER_DEV_HOST:-}" ]; then
    host="$AVOCADO_CONTAINER_DEV_HOST"
  elif [ "$LAB_VM" = 1 ]; then
    host="10.0.2.2"
  else
    host="<auto-detected>"
  fi
  printf 'bulk read %s:5599 (target pulls) | write %s (host pushes, loopback-bound) | control WS %s:5600' \
    "$host" "$(write_endpoint)" "$host"
}

# ---------------------------------------------------------------------------

cmd_setup() {
  [ "${LAB_VM:-1}" = 1 ] || die "LAB_VM=0: the target is real hardware, there is no VM to boot"
  ctx "SETUP the lab VM" \
    "runs on|this workstation" \
    "creates|QEMU VM '$SSH_ALIAS', ssh 127.0.0.1:2222, forwarded engine socket $DOCK_SOCK" \
    "note|first run builds and provisions a real Avocado OS runtime: minutes, needs network"
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
    "builder|$(build_desc)" \
    "runs on|$(target_engine_where)"

  pstep "1/4" "declare what to watch - on the laptop"
  pblock <<'EOF'

  avocado.yaml
      container_dev:
        images:
          - ref: my-app:dev        # the image you build
            service: app           # the unit that runs it

  That is the whole opt-in. No agent config, no registry URL.
EOF
  p ""
  p "  \$ docker buildx build --platform linux/arm64 -t my-app:dev ."

  local _t0=$SECONDS
  build_image "$version"
  BUILD_SECONDS=$(( SECONDS - _t0 ))
  pkv "built" "my-app:dev (arm64, built on the laptop)"

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
# always, not on-failure: --rm plus the default Type=simple means a container
# that exits cleanly - the app stopping, or an external "docker stop" - leaves
# the unit inactive with nothing to bring it back. on-failure covers only the
# crash case, so the demo target silently stops running the thing being demoed.
Restart=always

[Install]
WantedBy=multi-user.target
EOF
  ssh "$SSH_ALIAS" "systemctl daemon-reload && systemctl enable $APP_SERVICE >/dev/null 2>&1" \
    || die "could not install $APP_SERVICE on $SSH_ALIAS"

  # In native mode the image was built HERE, so nothing on the target is that image
  # until `seed` ships it - delivery IS the loop. Starting the unit and asserting a
  # baseline here can only work in vm mode, where the build happened on the target.
  #
  # There is deliberately no "unless the target already has it" escape. That escape
  # existed and was wrong: it tested whether the TAG was present, which a previous
  # run leaves behind pointing at a DIFFERENT image, so the demo restarted the unit
  # on a stale image and then failed asserting the version it had just built but
  # never delivered.
  if [ "$MODE" = vm ]; then
    ssh "$SSH_ALIAS" "systemctl restart $APP_SERVICE" || die "could not start $APP_SERVICE"
    sleep 4
    local line; line="$(app_line)"
    ctx "APP is up" "reading|$(target_engine_where)" "says|$line"
    case "$line" in
      "app $version "*) printf '   %-11s %s\n' "result" "baseline $version confirmed on the target" ;;
      *) die "app is not reporting '$version' - ssh $SSH_ALIAS 'journalctl -u $APP_SERVICE -n 20'" ;;
    esac
  elif ! present; then
    # Harness bookkeeping: it names the next action to run, which is exactly the
    # driver the recording must not show.
    printf '   %-11s %s\n' "unit" "installed and enabled, NOT started"
    printf '   %-11s %s\n' "why" "the host built $TEST_IMAGE; '$0 seed' delivers it over the loop"
    if [ -n "$(target_image_id)" ]; then
      printf '   %-11s %s\n' "note" "the target holds an older $TEST_IMAGE from a previous run; seed replaces it"
    fi
  fi
}

# Deliver the built image to the target through the real path, then start the unit.
#
# This is the step that proves delivery works at all. It needs a live session AND a
# running agent, because the push goes to the host's write listener and only the
# agent can pull it back down over the control WS.
cmd_seed() {
  # No version argument. It used to take one and assert it, but `seed` does not
  # build - it ships whatever the host holds under the watched tag - so the
  # argument was a claim about content that this step never established. Passing
  # `seed v1` after a `reload v2` failed with "app is not reporting 'v1'" while
  # delivery had in fact worked perfectly. The version is a property of the
  # artifact, so read it out of the artifact instead.
  # Gate first, then note. Printing the note before the session check emitted a
  # contextless line and then died, so `demo.sh seed v1` with no session led with
  # advice about an argument instead of the actual problem.
  session_running || die "no session - run '$0 up' first"
  local ignored="${1:-}"

  # Read the version from the image's LABEL, not by running the image. `seed` is a
  # ship-only step and must stay build-only: with TARGET_PLATFORM set the image is
  # a foreign architecture, and a `docker-container` buildx driver carries its
  # emulation inside the builder - so `demo.sh app v1` succeeds while `docker run`
  # of that same image fails "exec format error" unless binfmt/qemu-user happens to
  # be registered on the host. `image inspect` never executes anything, is cheaper,
  # and works over the forwarded socket in MODE=vm.
  local want
  want="$(build_engine image inspect --format '{{index .Config.Labels "org.avocado.demo.version"}}' "$TEST_IMAGE" 2>/dev/null | tr -d '\r\n')"
  # Both empty and the literal `<no value>` mean "no such label". Measured on docker
  # 29.7.1: a missing key yields EMPTY, whether or not the image carries other
  # labels, and an absent image exits non-zero with empty stdout - so the empty arm
  # is the one that fires here. `<no value>` is text/template's older output for a
  # missing map key; it is kept because this script deliberately supports daemons
  # back to 20.10 (see the builder-version gate in build_image), and it is NOT
  # verified on one. Do not drop it on the strength of a 29.x run alone.
  case "$want" in
    ''|'<no value>')
      die "$TEST_IMAGE on the build engine carries no org.avocado.demo.version label - rebuild it with '$0 app <version>'" ;;
  esac

  ctx "SEED the target with the baseline image" \
    "runs on|this workstation, then the target pulls" \
    "shipping|$TEST_IMAGE containing version $want" \
    "path|host build -> write listener $(write_endpoint) -> control WS -> agent pulls by digest -> $APP_SERVICE" \
    "why|native mode builds HERE, so the target has no image until the loop ships one"
  if [ -n "$ignored" ]; then
    printf '   %-11s %s\n' "note" "ignoring '$ignored' - seed ships what the host holds and reads the version off the image"
  fi

  pstep "3/4" "first deployment - laptop to device"
  local _s0; _s0="$(store_bytes)"

  if present; then
    p ""
    # `sync` blocks with no output and the device-side pull that follows is a
    # silent poll, so together they were ~15s of dead screen with nothing to
    # tell the viewer the demo had not hung. Push in the background and report
    # the blob store as it fills: that growth IS the wire transfer, so the
    # progress line and the payoff number are the same measurement.
    ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync >/dev/null 2>&1 ) &
    local _sync=$!
    while kill -0 "$_sync" 2>/dev/null; do
      printf '\r  %-18s %s' "pushing" "$(human $(( $(store_bytes) - _s0 )))"
      sleep 0.4
    done
    wait "$_sync" || die "container dev sync failed - is a session up? ($0 up)"
    # Overwrite the live line in place. The green line is the longer of the two,
    # so it covers the counter it replaces with no residue.
    printf '\r'
    pnum "pushed" "$(human $(( $(store_bytes) - _s0 )))" "the whole image, this one time"
  else
    ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync >/dev/null 2>&1 ) \
      || die "container dev sync failed - is a session up? ($0 up)"
  fi

  # Wait for the target to hold the HOST's image, not merely a tag of that name.
  # A stale tag from a previous run satisfies a presence check instantly, so this
  # loop used to fall through on the first tick and then restart the unit on the
  # old image.
  present || printf '   %-11s ' "waiting"
  local _w=0
  for _ in $(seq 1 30); do
    sleep 2
    _w=$(( _w + 2 ))
    if present; then printf '\r  %-18s %ss' "device pulling" "$_w"; else printf '.'; fi
    target_has_host_image && break
  done
  if present; then printf '\r  %-18s %s\n' "device pulled" "in ${_w}s"; else printf '\n'; fi
  # Report the versions, not the image IDs: the two sides report different kinds
  # of digest (see target_has_host_image), so printing them here sent the reader
  # chasing a mismatch that is expected and not the fault.
  target_has_host_image || die "the host's $TEST_IMAGE never reached the target (host has '$want', target reports '$(target_demo_version)') - check: $0 logs session ; $0 logs agent"

  # Label the wait before the ssh, not after: the restart call is itself a
  # multi-second round trip, so printing afterwards leaves exactly the stretch
  # of blank screen the label exists to cover.
  present && printf '  %-18s %s' "device" "restarting $APP_SERVICE"
  ssh "$SSH_ALIAS" "systemctl restart $APP_SERVICE" || die "could not start $APP_SERVICE"
  local line=""
  if present; then
    # Poll the app's own output rather than sleeping blind. The restart and the
    # settle ran to ~8s of motionless screen right before the payoff, which
    # reads as a stall; counting up says the demo is waiting on the device and
    # says how long it waited.
    local _r=0
    for _ in $(seq 1 30); do
      printf '\r  %-18s %s' "device" "restarting $APP_SERVICE  ${_r}s"
      line="$(app_line)"
      case "$line" in "app $want "*) break ;; esac
      _r=$(( _r + 1 ))
      sleep 1
    done
    printf '\r  %-18s %s\n' "device" "restarted $APP_SERVICE in ${_r}s"
  else
    sleep 4
    line="$(app_line)"
  fi
  ctx "APP is up" "reading|$(target_engine_where)" "says|$line"
  # Still a real check even though the image IDs already match: it is the
  # difference between the image having landed and the SERVICE having adopted it.
  #
  # Anchor on the leading `app <version> ` field rather than substring-matching the
  # whole line. write_ctx puts `image=$TEST_IMAGE` into the same line, so a bare
  # `*"$want"*` passes whenever the version is a substring of the image ref - and
  # TEST_IMAGE defaults to my-app:dev, so `want=dev` matched unconditionally. It
  # also let a prefix satisfy its own extension (v1 accepted while v10 ran).
  case "$line" in
    "app $want "*)
      if present; then
        # The figure was already shown in green as it accumulated; keep it here
        # only for the close, which contrasts it against the reload delta.
        FIRST_BYTES=$(( $(store_bytes) - _s0 ))
        p ""
        # The app's own line, minus the image field: step 1 already declared the
        # ref, and keeping it pushes this past the 64-column limit.
        local shown="${line% image=*}"
        p "  $shown"
        # Point at the proof rather than leaving it to be noticed. The caret run
        # is derived from the string so it stays aligned if the hostname changes.
        # The carets must sit under the HOSTNAME, not under the `running-on=`
        # label, so the prefix has to include the label itself.
        local host="${shown#*running-on=}"
        local pre="${shown%"$host"}"
        printf '  %*s' "${#pre}" ""
        printf '^%.0s' $(seq 1 "${#host}"); printf '\n'
        printf '  %*s%s\n' "${#pre}" "" "the board's own hostname"
        pbeat 2
      else
        printf '   %-11s %s\n' "result" "$want delivered over the loop and running on the target"
      fi ;;
    *) die "the target holds the host's image but its container still reports something else - ssh $SSH_ALIAS 'journalctl -u $APP_SERVICE -n 20'" ;;
  esac
}

cmd_up() {
  session_running && die "a session is already running - $0 down first"
  # Clear the log BEFORE the header reads it. write_endpoint() parses the bound
  # write port out of this file, and the session below truncates it on start - so
  # on a second run the header was reporting the PREVIOUS session's port as though
  # it were current. Truncating here makes write_endpoint fall back to saying no
  # session has bound one yet, which is the truth at this point in the run.
  : >"$UP_LOG"
  ctx "START the dev session" \
    "runs on|this workstation" \
    "serves|$(registry_endpoints)" \
    "store|$HOME/.avocado/container-dev/<runtime>/registry/" \
    "reaches|$SSH_ALIAS once over ssh to bootstrap it, then never again" \
    "log|$UP_LOG (every push shows up here)"
  # The CLI reads ./avocado.yaml from the cwd, not $AVOCADO_CONFIG.
  source_lab_env
  if [ "$MODE" = native ]; then
    # env.sh points DOCKER_HOST at the VM socket, and is_vm_routing_active() keys
    # on exactly that. Unset it or the CLI takes the vm path and builds/pushes
    # through the target's engine - the topology native mode exists to avoid.
    unset DOCKER_HOST
  fi
  # `setsid --fork`, with no trailing `&` and no subshell job.
  #
  # The previous form was `( setsid nohup CMD ... & )`. setsid execs in place when
  # it can, so the session stayed a child of this script and a bash job; the script
  # then blocked at exit waiting on it. Every step of `up` ran and printed, but the
  # script never returned - and when its output is piped (`demo.sh up | tail`) the
  # reader sees NOTHING at all, because the pipe's write end is still held. That
  # reads as "up hangs" when the session is in fact healthy.
  #
  # --fork makes setsid fork unconditionally, so the session is reparented away and
  # is never a job of this shell. Redirecting all three streams is still required:
  # an inherited stdout would keep the caller's pipe open on its own.
  # `${AVOCADO_BIN:-avocado}`, matching every other call site. Bare "$AVOCADO_BIN"
  # was an unbound variable under `set -u` whenever $LAB/env.sh was absent (its
  # source above is `[ -f ]`-guarded), and bash aborts the subshell BEFORE
  # performing the >"$UP_LOG" redirection - so the log kept a PREVIOUS session's
  # contents, and the `bulk listener` grep below then passed for a session that
  # never started. Verified: the redirect does not run, and with no `set -e` the
  # failing subshell does not stop the script either.
  ( cd "$SCRIPT_DIR" && setsid --fork "${AVOCADO_BIN:-avocado}" container dev up >"$UP_LOG" 2>&1 </dev/null )
  # Poll for the listener line rather than sleeping a fixed 15s. The grep below
  # is the real readiness test, so waiting on it directly is both correct and
  # roughly 11s faster - which is a tenth of the recording's runtime.
  for _ in $(seq 1 60); do
    grep -qE "bulk listener" "$UP_LOG" && break
    sleep 0.5
  done
  grep -qE "bulk listener" "$UP_LOG" || { tail -5 "$UP_LOG"; die "session did not come up - see $UP_LOG"; }

  pstep "2/4" "start dev mode - on the laptop"
  p ""
  p "  \$ avocado container dev up"
  pblock <<'EOF'

  laptop (x86-64)                     device (arm64)
  ┌──────────────────┐              ┌──────────────────┐
  │ docker build     │              │ dev agent        │
  │       │          │              │       │          │
  │       ▼          │ changed layer│       ▼          │
  │ layer store ─────┼─── over TLS ─┼──▶ pull it       │
  │       │          │              │       │          │
  │ file watcher ────┼──── notify ──┼──▶ restart app   │
  └──────────────────┘              └──────────────────┘
EOF
  p ""
  pkv "device" "bootstrapped over ssh, once"
  pkv "registry" "on the laptop; the device never reaches out"
  pbeat 3
  # The CLI's own startup lines carry the listener ports, the loopback write
  # port and the device IP on a single ~174-column line. Debug wants exactly
  # that; the recording cannot show it without running through the branding.
  if ! present; then
    sed -e 's/^/   /' <(tail -2 "$UP_LOG")
  fi
}

cmd_agent() {
  ctx "START the device agent" \
    "runs on|the HITL TARGET ($SSH_ALIAS), over ssh" \
    "unit|$AGENT_UNIT, shipped in the runtime by avocado-ext-container-agent-dev" \
    "pulls via|its own loopback proxy 127.0.0.1:15151 -> the host's bulk listener" \
    "restarts|$APP_SERVICE, and verifies container '$CONTAINER' (both set in the drop-in below)" \
    "gate|ConditionPathExists=/var/lib/avocado/container-dev/bootstrap.json, so '$0 up' must run first"

  # The unit stays inert until `container dev up` has delivered the bootstrap, so a
  # start before that is not an error - it is the condition doing its job. Say so
  # rather than reporting a failure the operator cannot act on.
  ssh "$SSH_ALIAS" "test -f /var/lib/avocado/container-dev/bootstrap.json" 2>/dev/null \
    || die "no bootstrap on the target yet - run '$0 up' first (the unit's ConditionPathExists gates on it)"

  # The agent learns BOTH of these only from its environment - nothing in the
  # bootstrap payload carries them.
  #
  # SERVICE is the unit that owns the container. Without it the agent falls back
  # to `docker restart <container>`, which re-executes the pinned image ID, so a
  # freshly pulled image is ignored while every layer reports success.
  #
  # CONTAINER is what the agent looks for AFTER restarting, to prove something is
  # actually running the image it just pulled. It defaults to `avocado-dev`, so
  # any deployment naming its container anything else - this lab names it after
  # APP_SERVICE - fails that check on every sync: "container avocado-dev does not
  # exist after 30s". The restart itself worked and the app is on the new image,
  # but the agent reports failure, never writes active-image.json, and retries in
  # a loop. setup-lab.sh installs the SERVICE half; this installs both, so it is
  # right on real hardware too, where setup-lab.sh never runs.
  #
  # mkdir -p, not install -d: the target's coreutils are BusyBox, which has no
  # `install`.
  # shellcheck disable=SC2087  # client-side expansion is intended: bake the names in
  ssh "$SSH_ALIAS" "mkdir -p /etc/systemd/system/$AGENT_UNIT.service.d && \
    cat > /etc/systemd/system/$AGENT_UNIT.service.d/10-service.conf" <<EOF
# Installed by demo.sh. Nothing in the bootstrap payload carries the owning unit
# or the container name, so both must be set here.
[Service]
Environment=AVOCADO_CONTAINER_DEV_SERVICE=$APP_SERVICE
Environment=AVOCADO_CONTAINER_DEV_CONTAINER=$CONTAINER
EOF
  ssh "$SSH_ALIAS" 'systemctl daemon-reload' \
    || die "could not reload systemd on $SSH_ALIAS after installing the agent drop-in"

  # Clear any start-limit counter before restarting. The unit widens systemd's
  # limit on purpose (StartLimitIntervalSec=300, Burst=5) so a genuinely broken
  # bootstrap lands in `failed` rather than looping invisibly - but this driver
  # restarts the agent once per run by design, so a few demo cycles inside five
  # minutes trip it and the next start is refused with "attempted too often".
  # Resetting here clears the counter for a deliberate restart; it does not mask
  # a failing unit, because the is-active poll below still has to pass.
  ssh "$SSH_ALIAS" "systemctl reset-failed $AGENT_UNIT 2>/dev/null; true" >/dev/null 2>&1

  # Restart rather than start: a re-run after a new session must not keep an agent
  # holding the previous session's pinned CA.
  ssh "$SSH_ALIAS" "systemctl restart $AGENT_UNIT" \
    || die "could not start $AGENT_UNIT - ssh $SSH_ALIAS 'journalctl -u $AGENT_UNIT -n 30'"
  # Poll instead of a fixed 5s: is-active below is the real test, and the unit
  # is usually up in well under a second.
  local active=unknown
  for _ in $(seq 1 20); do
    active="$(ssh "$SSH_ALIAS" "systemctl is-active $AGENT_UNIT" 2>/dev/null || echo unknown)"
    [ "$active" = active ] && break
    sleep 0.5
  done
  [ "$active" = active ] || die "$AGENT_UNIT is '$active' - ssh $SSH_ALIAS 'journalctl -u $AGENT_UNIT -n 30'"
  # Raw tracing lines - module paths, timestamps, proxy ports - are the highest
  # noise-per-pixel thing on screen and mean nothing to a viewer.
  if ! present; then
    ssh "$SSH_ALIAS" "journalctl -u $AGENT_UNIT --no-pager -n 3 -o cat" 2>/dev/null | sed -e 's/^/   /'
  fi
  pkv "agent" "connected, watching for changes"
}

cmd_reload() {
  local version="${1:-v2-RELOADED}"
  local before; before="$(app_line)"
  ctx "RELOAD: rebuild only, let the loop do the rest" \
    "mode|$MODE" \
    "builds on|$(build_engine_where)" \
    "version|$version${TARGET_PLATFORM:+   platform=$TARGET_PLATFORM}" \
    "pushes to|the host's write listener $(write_endpoint), tagged $(write_endpoint)/${TEST_IMAGE%%:*}" \
    "then|control WS notifies the target, which pulls by digest and restarts $APP_SERVICE" \
    "note|the unit is NOT touched here, so only the watcher path can move the container" \
    "before|$before"

  pstep "4/4" "change the code, rebuild - on the laptop"
  p ""
  p "  \$ docker buildx build --platform linux/arm64 -t my-app:dev ."
  pblock <<'EOF'

  That is the only command run. No push, no ssh, no device
  command, and nothing restarted by hand.
EOF
  pbeat 1.5

  local _s0; _s0="$(store_bytes)"
  local _t0=$SECONDS
  build_image "$version"
  BUILD_SECONDS=$(( SECONDS - _t0 ))

  if [ "$EMITS_TAG_EVENT" = 0 ]; then
    # Debug-only: this explains a Docker quirk, not the feature.
    if ! present; then
      printf '   %-11s %s\n' "trigger" "buildx emits no tag event, so triggering the sync explicitly"
    fi
    ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync >/dev/null 2>&1 ) \
      || die "container dev sync failed - is a session up? ($0 up)"
  fi

  local line=""
  _t0=$SECONDS
  if present; then
    # Stream the device's own log rather than printing progress dots. The app
    # emits a line every 2s, so this simultaneously shows that it is running on
    # the board (every line carries the board's hostname), that it was running
    # before and after, and how long the round trip took - countable at 2s a
    # line. Dots show none of that and cost the same wall time.
    p ""
    p "  the device's own log, live:"
    p ""
    local shown
    for _ in $(seq 1 45); do
      line="$(app_line)"
      shown="${line% image=*}"
      case "$line" in
        # The whole demo turns on this one line, and it arrives as the last of
        # half a dozen visually identical ones - so mark it. Printed directly
        # rather than through p(), which measures ${#s} and would count the
        # escape sequences against the 64-column budget.
        "app $version "*)
          printf '    \033[1;32m%s\033[0m  \033[32m<- new\033[0m\n' "$shown" ;;
        # Repeats are the point - a line per tick is what shows the app running
        # before and after, so print every one rather than deduplicating.
        "app "*)
          p "    $shown" ;;
      esac
      case "$line" in "app $version "*) break ;; esac
      sleep 2
    done
    SHIP_SECONDS=$(( SECONDS - _t0 ))
  else
    printf '   %-11s ' "waiting"
    for _ in $(seq 1 30); do
      sleep 2; printf '.'
      line="$(app_line)"
      case "$line" in "app $version "*) break ;; esac
    done
    printf '\n'
    SHIP_SECONDS=$(( SECONDS - _t0 ))
  fi
  RELOAD_BYTES=$(( $(store_bytes) - _s0 ))

  ctx "RESULT" \
    "reading|$(target_engine_where)" \
    "after|$line" \
    "pushes|$(count_in 'The push refers' "$UP_LOG") in $UP_LOG, $(count_in 'no basic auth credentials' "$UP_LOG") auth failures"
  case "$line" in
    "app $version "*)
      if present; then
        # The numbers land in the close block, which holds long enough to read.
        cmd_close "${FIRST_BYTES:-0}" "$RELOAD_BYTES" "$BUILD_SECONDS" "$SHIP_SECONDS"
      else
        printf '   %-11s %s\n' "result" "hot reload landed: the watcher moved the target to $version"
      fi ;;
    *) die "no reload after 90s - check: $0 logs session ; $0 logs agent" ;;
  esac
}

cmd_sync() {
  ctx "SYNC: re-push and notify, without waiting on an event" \
    "runs on|this workstation" \
    "pushes|whatever the BUILD engine currently holds under $TEST_IMAGE" \
    "caveat|if your image went to the other engine, this pushes the stale one and reports success"
  source_lab_env
  [ "$MODE" = native ] && unset DOCKER_HOST
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev sync ) || die "sync failed - is a session up?"
}

cmd_status() {
  local host_daemon target_daemon
  host_daemon="$(env -u DOCKER_HOST docker info --format '{{.Name}}' 2>/dev/null || echo '(unreachable)')"
  # How the target's engine is reached depends on the topology, and reporting the
  # wrong one reads as a fault. In native mode there is no forwarded socket at
  # all - the engine is reached over ssh - so probing $DOCK_SOCK there always
  # answered "(unreachable)" on a perfectly healthy board.
  local target_via
  if [ "$MODE" = native ]; then
    target_daemon="$(target_engine info --format '{{.Name}}' 2>/dev/null || true)"
    target_via="over ssh"
  else
    target_daemon="$(daemon_name "unix://$DOCK_SOCK")"
    target_via="via $DOCK_SOCK"
  fi
  : "${target_daemon:=(unreachable)}"

  ctx "WHERE THINGS ARE" \
    "workstation|engine '$host_daemon'  <- your builds land here if DOCKER_HOST is unset" \
    "HITL target|engine '$target_daemon' $target_via, shell via 'ssh $SSH_ALIAS'" \
    "registry|$(registry_endpoints)" \
    "store|$HOME/.avocado/container-dev/"

  local up_pid; up_pid="$(session_pids | head -1)"
  ctx "SESSION (workstation)" \
    "up|$([ -n "$up_pid" ] && echo "running (pid $up_pid)" || echo 'not running')" \
    "log|$UP_LOG" \
    "pushes|$(count_in 'The push refers' "$UP_LOG"), auth failures $(count_in 'no basic auth credentials' "$UP_LOG")"

  # Gate on ssh, not on the forwarded socket. Native mode has no forwarded socket by
  # design, so keying this block on the socket hid the target's whole state in the
  # default topology.
  if "${SSH_Q[@]}" -o ConnectTimeout=5 "$SSH_ALIAS" true 2>/dev/null; then
    ctx "TARGET ($SSH_ALIAS = $(target_hostname))" \
      "agent|$AGENT_UNIT $(ssh "$SSH_ALIAS" "systemctl is-active $AGENT_UNIT" 2>/dev/null || echo unknown)" \
      "service|$APP_SERVICE $(ssh "$SSH_ALIAS" "systemctl is-active $APP_SERVICE" 2>/dev/null || echo unknown)" \
      "app says|$(app_line)" \
      "running|$(ssh "$SSH_ALIAS" 'cat /var/lib/avocado/container-dev/active-image.json 2>/dev/null | tr -d "\n " ' 2>/dev/null || echo '(no pointer yet)')"
  else
    ctx "TARGET ($SSH_ALIAS)" "state|unreachable over ssh - run '$0 setup'"
  fi
}

cmd_logs() {
  case "${1:-}" in
    session)
      ctx "SESSION LOG" "from|this workstation" "file|$UP_LOG"
      tail -30 "$UP_LOG" ;;
    agent)
      ctx "AGENT LOG" "from|the HITL TARGET ($SSH_ALIAS)" "source|journalctl -u $AGENT_UNIT"
      ssh "$SSH_ALIAS" "journalctl -u $AGENT_UNIT --no-pager -n 30 -o cat" ;;
    app)
      ctx "APP LOG" "from|the HITL TARGET's engine ($SSH_ALIAS) via $DOCK_SOCK" "source|docker logs $CONTAINER"
      target_engine logs --tail 30 "$CONTAINER" ;;
    *) die "usage: $0 logs session|agent|app" ;;
  esac
}

cmd_down() {
  ctx "STOP the demo" "affects|this workstation (session) and the target (agent, app)" "keeps|the VM running and warm"
  source_lab_env
  ( cd "$SCRIPT_DIR" && "${AVOCADO_BIN:-avocado}" container dev down 2>/dev/null | sed -e 's/^/   /' ) || true
  session_pids | while read -r p; do kill "$p" 2>/dev/null; done
  ssh "$SSH_ALIAS" "systemctl stop $AGENT_UNIT $APP_SERVICE 2>/dev/null; docker stop $CONTAINER 2>/dev/null; docker rm $CONTAINER 2>/dev/null; true" >/dev/null 2>&1
  printf '   %-11s %s\n' "done" "session, agent and app stopped"
}

cmd_reset() {
  ctx "RESET to a pre-demo state" \
    "deletes|the HITL target's disk image and u-boot.rom under $VMDIR" \
    "deletes|the host registry store and the demo build context" \
    "keeps|the built runtime in the SDK volume - 'setup' reprovisions from it without a rebuild"
  session_pids | while read -r p; do kill "$p" 2>/dev/null; done
  sleep 2
  pkill -f "$DOCK_SOCK:" 2>/dev/null; rm -f "$DOCK_SOCK"
  if [ -f "$VMDIR/qemu.pid" ]; then
    local qp; qp="$(cat "$VMDIR/qemu.pid")"
    kill "$qp" 2>/dev/null
    for _ in $(seq 1 10); do kill -0 "$qp" 2>/dev/null || break; sleep 1; done
    kill -9 "$qp" 2>/dev/null
  fi
  rm -f "$VMDIR/avocado-os-"*.img "$VMDIR/u-boot.rom" "$VMDIR/console.log" "$VMDIR/qemu.pid"
  rm -rf "$HOME/.avocado/container-dev" "$BUILD_CTX"
  printf '   %-11s %s\n' "done" "start again with: $0 all"
}

# The thesis. With no narration a viewer who does not already know the feature
# watches v1 become v2 and concludes nothing, so the framing has to be on screen.
# The branded caption is a single ~38-char line and cannot carry it.
cmd_intro() {
  present || die "intro is presentation-only - run with PRESENT=1"
  # The blob store is the wire measurement, and has_blob() means a blob left by
  # an earlier run is never re-sent - so a second recording would report a first
  # delivery of nearly nothing. Refuse rather than headline a false number.
  local sz; sz="$(store_bytes)"
  if [ "${sz:-0}" -gt 0 ]; then
    die "the layer store is not empty ($(human "$sz")) - the delivery figures would understate the transfer. Clear $STORE_ROOT and re-run."
  fi
  # A recording harness leaves its own invocation on the first line of the frame.
  # Wipe it: the viewer should open on the title card, not on the path to the
  # script that drove the demo.
  printf '\033[2J\033[H'
  pblock <<'EOF'

════════════════════════════════════════════════════════════════
  AVOCADO OS  ·  CONTAINER DEV MODE

  Change a container on a running device in seconds - with no
  registry, and without reflashing the device.
════════════════════════════════════════════════════════════════
EOF
  p ""
  pkv "laptop" "x86-64 - Docker - where you already build"
  pkv "device" "NXP FRDM-IMX93 v1.2 - arm64 - Avocado OS"
  pkv "app" "run by app.service on the device"
  pblock <<'EOF'

  Normally a one-line change means: rebuild the image, push it
  to a registry, pull all of it down again, restart. The cost
  is set by the image size, not by the size of the change.
EOF
  pbeat 3
}

# The close carries the numbers. The branded end card carries the logo and the
# caption; duplicating either here would waste the only screen the viewer is
# likely to remember.
cmd_close() {
  present || die "close is presentation-only - run with PRESENT=1"
  local first="${1:-0}" delta="${2:-0}" tbuild="${3:-0}" tship="${4:-0}"
  prule "════════════════════════════════════════════════════════════════"
  p "  LANDED"
  p ""
  pnum "changed layer" "$(human "$delta")" "moved over the wire"
  pnum "already there" "$(human "$first")" "not re-sent"
  p ""
  # A cached rebuild genuinely takes under a second - only the top layer changes -
  # but printing "0s" reads as a broken measurement rather than a fast one.
  local b="${tbuild}s"; [ "${tbuild:-0}" -eq 0 ] && b="<1s"
  pnum "build" "$b"
  pnum "deliver + restart" "${tship}s"
  p ""
  p "  the device was never rebooted, and never reflashed"
  prule "════════════════════════════════════════════════════════════════"
  pbeat 4
}

# The whole recorded demo, in ONE process.
#
# Not a sequence of separate `demo.sh <step>` calls: the delivery figures are
# measured in cmd_seed and reported in the close after cmd_reload, so they have
# to share a shell. Running it as one action also keeps the harness off screen -
# the recording shows a demo, not a driver invoking itself six times.
cmd_present() {
  present || die "run with PRESENT=1"
  local v1="${1:-v1}" v2="${2:-v2}"
  cmd_intro
  cmd_app "$v1"
  cmd_up
  cmd_agent
  cmd_seed          # no version: it reads the one cmd_app just built
  cmd_reload "$v2"  # ends by printing the close block with the measured numbers
}

cmd_all() {
  local v1="${1:-v1}" v2="${2:-v2-RELOADED}"
  [ "${LAB_VM:-1}" = 1 ] && cmd_setup
  # Order matters and is not arbitrary: the unit can only run an image the target
  # actually has, and in native mode only the session+agent can put one there. So
  # build and install first, bring the loop up, then seed through it, then reload.
  cmd_app "$v1"
  cmd_up
  cmd_agent
  # No version passed: seed reads it out of the image cmd_app just built.
  cmd_seed
  cmd_reload "$v2"
  cmd_status
}

case "${1:-}" in
  setup)  shift; cmd_setup "$@" ;;
  verify) shift; cmd_verify "$@" ;;
  app)    shift; cmd_app "$@" ;;
  seed)   shift; cmd_seed "$@" ;;
  up)     shift; cmd_up "$@" ;;
  agent)  shift; cmd_agent "$@" ;;
  reload) shift; cmd_reload "$@" ;;
  sync)   shift; cmd_sync "$@" ;;
  status) shift; cmd_status "$@" ;;
  logs)   shift; cmd_logs "$@" ;;
  down)   shift; cmd_down "$@" ;;
  reset)  shift; cmd_reset "$@" ;;
  all)    shift; cmd_all "$@" ;;
  intro)  shift; cmd_intro "$@" ;;
  present) shift; cmd_present "$@" ;;
  close)  shift; cmd_close "$@" ;;
  ""|-h|--help|help)
    # Print the header comment block: from line 3 until the first non-comment line.
    awk 'NR>=3 && /^#/ { sub(/^# ?/, ""); print; next } NR>=3 { exit }' "${BASH_SOURCE[0]}"
    ;;
  *) die "unknown command '${1}' - run '$0 help'" ;;
esac
