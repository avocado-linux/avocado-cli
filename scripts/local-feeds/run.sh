#!/usr/bin/env bash
# Local integration test for named feeds (`repos:` / `distro.feeds`).
#
# No AWS, no Connect. The only network use is `avocado sdk install`, which every
# dnf passthrough needs (they die on a missing rpmrc before any HTTP otherwise);
# the feeds under test are all local. Proves:
#   1. a `path:` feed (dir of RPMs) and a Basic-auth `url:` feed both resolve
#      from inside the SDK container (the url one via the loopback rewrite)
#   2. `distro.feeds` order is dnf priority: dnf's own transaction picks the
#      EARLIER feed's OLDER version over the LATER feed's NEWER one
#   3. `stages:` scoping through the real container plumbing: a feed limited to
#      [sdk, ext] is invisible at the runtime stage and visible at sdk and ext
#   4. the canonical document exists, records credential identity, holds no secret
#
# Requires: docker, rpmbuild, createrepo_c, python3. Uses this worktree's debug
# build unless $AVOCADO points elsewhere.
#
#   scripts/local-feeds/run.sh [workdir]
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
AVOCADO=${AVOCADO:-$HERE/../../target/debug/avocado}
WORK=${1:-$PWD/.local-feeds-test}
PORT=${PORT:-18080}
TARGET=${TARGET:-qemux86-64}
USER_=tester
PASS_=s3cret

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "ok   $*"; }

[ -x "$AVOCADO" ] || fail "avocado binary not found at $AVOCADO (cargo build first)"
for t in docker rpmbuild createrepo_c python3; do command -v "$t" >/dev/null || fail "$t not installed"; done

rm -rf "$WORK"; mkdir -p "$WORK"/{rpmbuild,feed-a,feed-b,project}
cd "$WORK"

# --- two versions of a throwaway noarch RPM -------------------------------
mkrpm() { # name version outdir
  cat > "rpmbuild/$1-$2.spec" <<SPEC
Name: $1
Version: $2
Release: 1
Summary: local-feeds test package
License: MIT
BuildArch: noarch
%description
Marker package for the avocado local feeds test.
%install
mkdir -p %{buildroot}/usr/share/$1
echo "$2" > %{buildroot}/usr/share/$1/version
%files
/usr/share/$1
SPEC
  rpmbuild -bb --quiet --define "_topdir $WORK/rpmbuild" "rpmbuild/$1-$2.spec" >/dev/null
  cp "$WORK/rpmbuild/RPMS/noarch/$1-$2-1.noarch.rpm" "$3/"
}
mkrpm hello-feed 1.0 feed-a     # the "local build": OLDER, listed FIRST
mkrpm hello-feed 2.0 feed-b     # the "vendor" feed: NEWER, listed LAST, ext-only
createrepo_c --quiet feed-a
createrepo_c --quiet feed-b
pass "built hello-feed 1.0 (feed-a) and 2.0 (feed-b) with repodata"

# --- Basic-auth server = the private-feed stand-in ------------------------
python3 "$HERE/authserve.py" "$WORK/feed-b" "$PORT" "$USER_" "$PASS_" 2> "$WORK/authserve.log" &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
sleep 0.5
curl -fsS -u "$USER_:$PASS_" "http://127.0.0.1:$PORT/repodata/repomd.xml" >/dev/null || fail "auth server not serving"
curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/repodata/repomd.xml" | grep -q 401 || fail "auth server accepted an anonymous request"
pass "auth server up on :$PORT (401 anonymous, 200 with creds)"

# --- the project ------------------------------------------------------------
cat > project/avocado.yaml <<YAML
default_target: $TARGET
supported_targets: [$TARGET]

distro:
  release: 2026
  channel: next
  # order = priority: local-build shadows everything, vendor is last
  feeds: [local-build, avocado, vendor]

repos:
  local-build:
    path: ../feed-a
  vendor:
    url: http://localhost:$PORT
    username: '{{ env.VENDOR_USER }}'
    password: '{{ env.VENDOR_PASS }}'
    stages: [sdk, ext]

runtimes:
  dev:
    packages: {}

extensions:
  app:
    types: [sysext]
    version: "0.1.0"

sdk:
  image: "docker.io/avocadolinux/sdk:2026"
  container_args: [--network=host]
YAML
cd project
export VENDOR_USER=$USER_ VENDOR_PASS=$PASS_
QF='%{name}-%{evr}@%{repoid}'

# 0. bootstrap — the one network step. Every dnf passthrough needs the SDK
#    sysroot (RPM_CONFIGDIR lives under it), and this also seeds the rootfs
#    rpmdb the runtime/ext stages copy from.
echo "bootstrapping SDK (network) ..."
"$AVOCADO" --no-tui sdk install --force > sdk-install.out 2>&1 || { tail -40 sdk-install.out >&2; fail "sdk install failed"; }
pass "sdk install"

# 1. sdk stage: both feeds visible — path via bind mount, vendor via Basic auth
#    through the loopback rewrite (localhost -> host.docker.internal)
"$AVOCADO" --no-tui sdk dnf repoquery --qf "$QF" hello-feed > sdk.out 2> sdk.err \
  || { cat sdk.err >&2; fail "sdk dnf repoquery failed"; }
grep -q 'hello-feed-1.0-1@local-build' sdk.out || { cat sdk.out; fail "path: feed not visible"; }
grep -q 'hello-feed-2.0-1@vendor'      sdk.out || { cat sdk.out; fail "Basic-auth url: feed not visible"; }
grep -q ' 200 ' ../authserve.log || fail "no authenticated request reached the vendor feed"
pass "sdk stage: path feed + Basic-auth url feed both resolve"

# 2. canonical document
DOC=.avocado/feeds/$TARGET.json
[ -f "$DOC" ] || fail "canonical document $DOC missing"
grep -q '"version": 1' "$DOC" || fail "canonical doc has no version"
grep -q '"any_credentialed": true' "$DOC" || fail "canonical doc does not flag the credentialed feed"
grep -q '"any_project_local": true' "$DOC" || fail "canonical doc does not flag the path: feed"
grep -q "\"credential_identity\": \"$USER_\"" "$DOC" || fail "canonical doc lacks credential identity"
grep -q "$PASS_" "$DOC" && fail "SECRET LEAKED into canonical document"
grep -q 'host.docker.internal' "$DOC" || fail "loopback URL was not rewritten"
pass "canonical document: versioned, flags set, identity recorded, no secret"

# 3. ordering: dnf must resolve hello-feed to local-build's 1.0, not vendor's 2.0.
#    `sdk dnf` has no --installroot, so this lands in the throwaway container;
#    dnf's transaction table is the proof.
"$AVOCADO" --no-tui sdk dnf install -y hello-feed > install.out 2> install.err \
  || { cat install.err >&2; fail "sdk dnf install failed"; }
grep -Eq 'hello-feed[[:space:]]+noarch[[:space:]]+1\.0-1[[:space:]]+local-build' install.out \
  || { cat install.out; fail "priority not honored: expected 1.0 from local-build in the transaction"; }
pass "ordering: dnf chose local-build 1.0 over vendor 2.0"

echo
echo "dnf User-Agent seen by the vendor feed (for Phase 0):"
grep -o "ua='[^']*'" ../authserve.log | sort -u | head -3
echo

# 4. stage scoping through the real plumbing
"$AVOCADO" --no-tui runtime dnf -r dev repoquery --qf "$QF" hello-feed > runtime.out 2> runtime.err \
  || { cat runtime.err >&2; fail "runtime dnf repoquery failed"; }
grep -q '@local-build' runtime.out || { cat runtime.out; fail "path: feed missing at runtime stage"; }
grep -q '@vendor' runtime.out && fail "[sdk, ext]-scoped vendor feed leaked into the runtime stage"
pass "stage scoping: runtime stage sees local-build only"
"$AVOCADO" --no-tui ext dnf -e app repoquery --qf "$QF" hello-feed > ext.out 2> ext.err \
  || { cat ext.err >&2; fail "ext dnf repoquery failed"; }
grep -q '@vendor' ext.out || { cat ext.out; fail "vendor feed missing at ext stage"; }
pass "stage scoping: ext stage sees vendor"

echo "ALL PASSED  (workdir: $WORK)"
