#!/usr/bin/env bash
# How the CLI behaves when a package repository answers HTTP 429: a real CLI
# and a real dnf, cold `avocado sdk install` runs through ratelimit_proxy.py in
# front of the real feed. dnf reports a 429 as "No more mirrors to try"; this
# checks that the CLI says what happened instead, and that anonymous,
# logged-in and CI clients identify to the repository as expected:
#   - the User-Agent each one sends,
#   - that identified clients are counted apart from a shared address,
#   - that a 429 fails with one message naming the wait.
#
# Limits here are scaled to one install (~750 requests). Needs docker and
# network access to the feed; takes about ten minutes. Run
# `ratelimit_proxy.py --self-test` for the fast check.
set -uo pipefail
cd "$(dirname "$0")"
HERE=$PWD
ROOT=$(cd ../.. && pwd)
AVOCADO=${AVOCADO:-$ROOT/target/debug/avocado}
UPSTREAM=${UPSTREAM:-https://repo.avocadolinux.org}
TARGET=${TARGET:-qemux86-64}
WORK=$ROOT/.local-feeds-test/ratelimit
WINDOW=120 # long enough that one install stays inside one window

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "ok   $*"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL $*"; }
die() { echo "error: $*" >&2; exit 1; }
check() { [[ $2 == "$3" ]] && ok "$1" || bad "$1 (expected $2, got $3)"; }

[[ -x $AVOCADO ]] || die "no avocado binary at $AVOCADO (cargo build first)"
python3 -I "$HERE/ratelimit_proxy.py" --self-test >/dev/null || die "ratelimit_proxy.py --self-test failed"

cleanup() { [[ -n ${PROXY_PID:-} ]] && kill "$PROXY_PID" 2>/dev/null; }
trap cleanup EXIT

rm -rf "$WORK"; mkdir -p "$WORK"
( cd "$WORK" && "$AVOCADO" init --target "$TARGET" proj >/dev/null ) || die "avocado init failed"
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1])')
# The host's routable address, not loopback: the CLI rewrites loopback to the
# docker bridge, so dnf and the CLI's own requests would reach ratelimit_proxy.py from two
# different addresses and per-address counting would split. In production both
# leave through the same public address; this keeps the rig the same.
HOST_IP=${HOST_IP:-$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')}
[[ -n $HOST_IP ]] || die "cannot find a routable host address; set HOST_IP"

# run_case NAME "waf args" [VAR=value ...] -- one cold install; leaves
# $WORK/NAME.out (CLI output), $WORK/NAME.log (one JSON line per request).
run_case() {
  local name=$1 waf_args=$2; shift 2
  local home=$WORK/home-$name
  mkdir -p "$home/.config"
  [[ -n ${CREDENTIALS:-} ]] && { mkdir -p "$home/.config/avocado-connect"; \
    printf '%s' "$CREDENTIALS" > "$home/.config/avocado-connect/credentials.json"; }
  ( cd "$WORK/proj" && "$AVOCADO" clean -f >/dev/null 2>&1 )
  # shellcheck disable=SC2086
  python3 -I "$HERE/ratelimit_proxy.py" --upstream "$UPSTREAM" --port "$PORT" --window $WINDOW \
    --log "$WORK/$name.log" $waf_args 2>/dev/null &
  PROXY_PID=$!
  sleep 1
  ( cd "$WORK/proj" && env -u AVOCADO_CONNECT_TOKEN HOME="$home" XDG_CONFIG_HOME="$home/.config" \
      AVOCADO_REPO_URL="http://$HOST_IP:$PORT" "$@" \
      timeout 1500 "$AVOCADO" sdk install --no-tui >"$WORK/$name.out" 2>&1 )
  echo $? >"$WORK/$name.exit"
  kill "$PROXY_PID" 2>/dev/null; wait "$PROXY_PID" 2>/dev/null; PROXY_PID=
}

# q NAME EXPR -- evaluate EXPR over the request log L (list of dicts).
q() { python3 -I -c "import json,sys; L=[json.loads(l) for l in open(sys.argv[1])]; print($2)" "$WORK/$1.log"; }
exited() { [[ $(cat "$WORK/$1.exit") == 0 ]] && echo success || echo failure; }
says()   { grep -c "$2" "$WORK/$1.out"; }

key_of() { python3 -c 'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest()[:12])' "$1"; }
CI_TOKEN=rig-ci-token
PROFILE_TOKEN=rig-profile-token
PROFILE='{"default_profile":"default","profiles":{"default":{"api_url":"https://connect.invalid","token":"'$PROFILE_TOKEN'","user":{"email":"rig@example.invalid","name":"rig"},"created_at":"2026-01-01T00:00:00Z"}}}'
ENFORCE="--enforce --limit-backstop 100000"
MSG='HTTP 429 (too many requests)'

echo "# 1. anonymous, under every limit"
run_case anon "$ENFORCE --limit-anon 100000 --limit-tier 1:100000"
check "anonymous install succeeds"                   success "$(exited anon)"
check "every request is anonymous to the repository"       True "$(q anon 'all(x["ua"].startswith("avocado-cli/") and ";key/" not in x["ua"] for x in L)')"
check "it is counted per address (no tier rule hit)" 0 "$(q anon 'sum(x["rule"].startswith("Tier") for x in L)')"

echo "# 2. anonymous, over the anonymous limit"
run_case anon-limited "$ENFORCE --limit-anon 200 --limit-tier 1:100000"
check "the install fails"                            failure "$(exited anon-limited)"
check "the repository answered 429 from the anonymous rule" True "$(q anon-limited 'any(x["status"]==429 and x["rule"]=="Global" for x in L)')"
check "the user is told why, once"                   1 "$(says anon-limited "$MSG")"
check "the message mentions avocado login"              1 "$(grep "$MSG" "$WORK/anon-limited.out" | grep -c 'avocado login')"
check "the message mentions AVOCADO_CONNECT_TOKEN"      1 "$(grep "$MSG" "$WORK/anon-limited.out" | grep -c AVOCADO_CONNECT_TOKEN)"

echo "# 3. logged in (stored profile), same anonymous limit"
CREDENTIALS=$PROFILE run_case login "$ENFORCE --limit-anon 200 --limit-tier 1:100000"
check "a logged-in install succeeds under the anonymous limit" success "$(exited login)"
check "the repository sees the profile's key and tier/1"   True "$(q login 'all(x["ua"].endswith(";key/'"$(key_of $PROFILE_TOKEN)"';tier/1") for x in L)')"
check "no request fell to the anonymous rule"        0 "$(q login 'sum(x["rule"]=="Global" for x in L)')"

echo "# 4. CI (AVOCADO_CONNECT_TOKEN, no profile), same anonymous limit"
run_case ci "$ENFORCE --limit-anon 200 --limit-tier 1:100000" AVOCADO_CONNECT_TOKEN=$CI_TOKEN
check "a CI install succeeds under the anonymous limit" success "$(exited ci)"
check "the repository sees the CI token's key and tier/1"  True "$(q ci 'all(x["ua"].endswith(";key/'"$(key_of $CI_TOKEN)"';tier/1") for x in L)')"

echo "# 5. CI token takes precedence over a stored profile"
CREDENTIALS=$PROFILE run_case ci-over-profile "$ENFORCE --limit-anon 100000 --limit-tier 1:100000" AVOCADO_CONNECT_TOKEN=$CI_TOKEN
check "the repository sees the CI token, not the profile"  True "$(q ci-over-profile 'all(";key/'"$(key_of $CI_TOKEN)"';" in x["ua"] for x in L)')"

echo "# 6. identified, over its tier limit"
run_case ci-limited "$ENFORCE --limit-anon 100000 --limit-tier 1:200" AVOCADO_CONNECT_TOKEN=$CI_TOKEN
check "the install fails"                            failure "$(exited ci-limited)"
check "the repository answered 429 from the tier rule"     True "$(q ci-limited 'any(x["status"]==429 and x["rule"]=="Tier1" for x in L)')"
check "the user is told why, once"                   1 "$(says ci-limited "$MSG")"
check "a logged-in user is not told to log in"       0 "$(grep "$MSG" "$WORK/ci-limited.out" | grep -c 'avocado login')"

echo "# 7. a per-address ceiling still applies to identified clients"
run_case backstop "--enforce --limit-anon 100000 --limit-tier 1:100000 --limit-backstop 200" AVOCADO_CONNECT_TOKEN=$CI_TOKEN
check "the install fails"                            failure "$(exited backstop)"
check "the backstop answered 429"                    True "$(q backstop 'any(x["status"]==429 and x["rule"]=="IpBackstop" for x in L)')"

echo
echo "$PASS passed, $FAIL failed (logs in $WORK)"
[[ $FAIL == 0 ]]
