#!/usr/bin/env bash
#
# verify-vm-write-path.sh - Validate Container Dev Mode task 7.1:
# the authenticated VM write path + CA delivery (design D2/H4).
#
# Run this on the HOST (the machine running `avocado container dev up`), with a
# booted avocado-vm engine reachable over SSH and the host docker CLI routed at
# it (DOCKER_HOST -> avocado-vm dockerd, so is_vm_routing_active() is true).
#
# It asserts the four falsifiable properties task 7.1 requires:
#   1. The per-project CA is DELIVERED at `up` into the VM engine's per-connection
#      docker trust store (/etc/docker/certs.d/10.0.2.2:<write-port>/ca.crt), not
#      baked. (falsifier: VM CA is a build-time static overlay file)
#   2. A guest push to 10.0.2.2:<write-port> over authenticated HTTPS SUCCEEDS.
#   3. An unauthenticated write to that listener is REFUSED (401), so the write
#      path is not anonymous. (falsifier: guest write path unauthenticated / A3)
#   4. The avocado-vm overlay bakes NO CA - it only provisions /etc/container-dev.
#
# Every step prints PASS/FAIL; a non-zero exit means the verify failed.

set -euo pipefail

# ---------------------------------------------------------------------------
# Config - override via env. The :? entries are required; the rest have defaults.
# ---------------------------------------------------------------------------
AVOCADO_BIN="${AVOCADO_BIN:-avocado}"
: "${AVOCADO_CONTAINER_DEV_VM:?set to <user@host> of the avocado-vm engine guest}"
: "${AVOCADO_CONTAINER_DEV_DEVICE:?set to <user@host> of the QEMU device}"
: "${DOCKER_HOST:?set to the avocado-vm dockerd socket so is_vm_routing_active() is true}"
WRITE_PORT="${AVOCADO_CONTAINER_DEV_WRITE_PORT:-5601}"
CONFIG="${AVOCADO_CONFIG:-avocado.yaml}"
# A trivial watched image whose ref matches runtimes.<name>.container_dev.images[].ref
TEST_IMAGE="${TEST_IMAGE:-my-app:dev}"
# Path to the meta-avocado base-files bbappend that provisions the trust-store dir
# (used only for the "no static CA baked" source check). Adjust to your checkout.
BBAPPEND="${BBAPPEND:-$HOME/repos/work/peridio-scarthgap-build/meta-avocado/meta-avocado-qemu/recipes-core/base-files/base-files_%.bbappend}"

VM_REGISTRY="10.0.2.2:${WRITE_PORT}"
GUEST_CA="/etc/docker/certs.d/${VM_REGISTRY}/ca.crt"
# The host-side registry store the write listener persists blobs/manifests into.
STORE_ROOT="${AVOCADO_CONTAINER_DEV_STORE:-$HOME/.avocado/container-dev}"

# The CLI loads its config as the relative path "avocado.yaml" from the working
# directory (it does not honor $AVOCADO_CONFIG), so run every avocado invocation
# from the directory that holds the config.
CONFIG="$(readlink -f "$CONFIG")"
cd "$(dirname "$CONFIG")"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"; "$AVOCADO_BIN" container dev down >/dev/null 2>&1 || true' EXIT

pass=0
fail=0
ok() {
  echo "  PASS: $*"
  pass=$((pass + 1))
}
bad() {
  echo "  FAIL: $*"
  fail=$((fail + 1))
}
step() {
  echo
  echo "== $* =="
}

# ---------------------------------------------------------------------------
step "0. Preflight"
# ---------------------------------------------------------------------------
command -v "$AVOCADO_BIN" >/dev/null || {
  echo "avocado binary '$AVOCADO_BIN' not found"
  exit 2
}
# Hermetic run: clear any prior registry store so a manifest present after sync
# proves THIS run's guest push landed (the store persists across runs).
"$AVOCADO_BIN" container dev down >/dev/null 2>&1 || true
rm -rf "$STORE_ROOT"
if ssh -o BatchMode=yes "$AVOCADO_CONTAINER_DEV_VM" 'docker version >/dev/null 2>&1'; then
  ok "avocado-vm reachable and docker responds"
else
  bad "avocado-vm unreachable or docker not running on it"
fi
if grep -q 'container_dev' "$CONFIG"; then
  ok "$CONFIG carries a container_dev block"
else
  bad "$CONFIG has no container_dev block (feature off)"
fi

# ---------------------------------------------------------------------------
step "4. No static CA baked into the avocado-vm overlay (design D8/H4)"
# ---------------------------------------------------------------------------
# The overlay must only provision the trust-store LOCATION - never a CA. Check the
# base-files bbappend source: it must create /etc/container-dev and install no cert.
if [ -f "$BBAPPEND" ]; then
  if grep -Eq 'install .*(\.crt|\.pem|ca-cert|ca\.crt)' "$BBAPPEND"; then
    bad "the base-files bbappend installs a certificate - a CA is baked ($BBAPPEND)"
  else
    ok "the base-files bbappend bakes no CA (provisions the location only)"
  fi
  if grep -q 'container-dev' "$BBAPPEND"; then
    ok "the overlay provisions the /etc/container-dev trust-store location"
  else
    bad "the overlay does not provision /etc/container-dev"
  fi
else
  echo "  SKIP: bbappend not found at $BBAPPEND (set BBAPPEND to your checkout)"
fi

# ---------------------------------------------------------------------------
step "1. Bring the session up (delivers the CA, binds the write listener)"
# ---------------------------------------------------------------------------
echo "  running: $AVOCADO_BIN container dev up   (background)"
"$AVOCADO_BIN" container dev up >"$TMP/up.log" 2>&1 &
UP_PID=$!
# Wait for the write listener + CA delivery to settle (bootstrap is one-shot at up).
for _ in $(seq 1 30); do
  if ssh -o BatchMode=yes "$AVOCADO_CONTAINER_DEV_VM" "test -f '$GUEST_CA'" 2>/dev/null; then
    break
  fi
  kill -0 "$UP_PID" 2>/dev/null || {
    echo "  up exited early; log:"
    sed 's/^/    /' "$TMP/up.log"
    exit 2
  }
  sleep 1
done

# ---------------------------------------------------------------------------
step "1/4. CA delivered into the VM engine trust store at run time"
# ---------------------------------------------------------------------------
if ssh -o BatchMode=yes "$AVOCADO_CONTAINER_DEV_VM" \
  "openssl x509 -in '$GUEST_CA' -noout -subject" >"$TMP/ca.txt" 2>/dev/null; then
  ok "delivered CA present + valid at $GUEST_CA on the guest ($(cat "$TMP/ca.txt"))"
else
  bad "no valid CA at $GUEST_CA on the guest - deliver_vm_ca did not run"
fi

# ---------------------------------------------------------------------------
step "3. Write path is authenticated - an unauthenticated write is refused"
# ---------------------------------------------------------------------------
# The write listener is loopback-bound on the host at 127.0.0.1:<write-port>
# (the guest reaches the same socket via 10.0.2.2). An unauthenticated manifest
# PUT must be refused (Basic write token required, not anonymous / A3).
code="$(curl -sk -o /dev/null -w '%{http_code}' -X PUT \
  "https://127.0.0.1:${WRITE_PORT}/v2/verify-7-1/manifests/dev" 2>/dev/null || echo 000)"
if [ "$code" = "401" ]; then
  ok "unauthenticated write refused with 401 (Basic write token required)"
else
  bad "unauthenticated write returned $code, expected 401 (write path not authenticated)"
fi

# ---------------------------------------------------------------------------
step "2. Guest push over authenticated HTTPS SUCCEEDS"
# ---------------------------------------------------------------------------
# Build the watched image on the VM engine, then let the CLI push it to the
# routable HTTPS write listener with the delivered CA + Basic write token.
printf 'FROM busybox:latest\nRUN echo verify-7.1 > /marker\n' >"$TMP/Dockerfile"
if docker build -t "$TEST_IMAGE" "$TMP" >"$TMP/build.log" 2>&1; then
  ok "built watched image $TEST_IMAGE on the VM engine"
else
  bad "failed to build $TEST_IMAGE (see below)"
  sed 's/^/    /' "$TMP/build.log"
fi
echo "  running: $AVOCADO_BIN container dev sync"
"$AVOCADO_BIN" container dev sync >"$TMP/sync.log" 2>&1
# `sync` only SIGNALs the running `up` to re-push; the guest `docker push` over
# HTTPS then runs asynchronously in `up`. A zero exit from `sync` proves the
# signal was sent, NOT that a blob landed - so wait for the manifest tag to
# appear in the registry store (cleared at preflight), which is the real proof
# the authenticated HTTPS push to $VM_REGISTRY succeeded.
tag="${TEST_IMAGE##*:}"
landed=0
for _ in $(seq 1 25); do
  if find "$STORE_ROOT" -path "*/registry/manifests/tags/$tag" 2>/dev/null | grep -q .; then
    landed=1
    break
  fi
  sleep 1
done
if [ "$landed" = 1 ]; then
  ok "guest push landed: manifest tag '$tag' present in the registry store (authenticated HTTPS push to $VM_REGISTRY succeeded)"
else
  bad "guest push did not land: no manifest tag '$tag' in $STORE_ROOT after sync"
  echo "    -- up.log tail --"
  tail -15 "$TMP/up.log" 2>/dev/null | sed 's/^/    /'
fi

# ---------------------------------------------------------------------------
step "Verdict"
# ---------------------------------------------------------------------------
echo "  passed: $pass   failed: $fail"
if [ "$fail" -eq 0 ]; then
  echo "  RESULT: 7.1 VM write path VERIFIED"
  exit 0
fi
echo "  RESULT: 7.1 VM write path FAILED - see the FAIL lines above"
exit 1
