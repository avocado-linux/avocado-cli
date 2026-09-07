#!/usr/bin/env bash
# End-to-end proof that an `org:` feed works: the CLI mints a short-lived token
# from a stand-in Connect, injects it into a generated .repo, and dnf installs a
# package from a feed that refuses anonymous access.
#
# The stand-in is scripts/local-feeds/edge.py, which implements the same
# contract Connect and the edge lambda will (see edge-contract.md). When the real
# ones exist this script changes by one variable: AVOCADO_CONNECT_URL.
#
# Skips cleanly when python's `cryptography` is unavailable, so the stdlib-only
# rig (run.sh) stays runnable everywhere.
set -uo pipefail
cd "$(dirname "$0")"
HERE=$PWD
ROOT=$(cd ../.. && pwd)
AVOCADO=${AVOCADO:-$ROOT/target/debug/avocado}
# Under .local-feeds-test/, which is already gitignored: a scratch directory in
# the repo root shows up in `git status` for anyone who runs the rig.
WORK=$ROOT/.local-feeds-test/org-feed
ORG=${ORG:-acme}
REL=2026
BRANCH=main
TARGET=${TARGET:-qemux86-64}
PAT=test-pat

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "ok   $*"; }
bad() { FAIL=$((FAIL+1)); echo "FAIL $*"; }
die() { echo "error: $*" >&2; exit 1; }

python3 -c 'import cryptography' 2>/dev/null \
  || { echo "skipped: python3 `cryptography` is not installed (needed for ES256)"; exit 0; }
[[ -x $AVOCADO ]] || die "no avocado binary at $AVOCADO (cargo build first)"

cleanup() { [[ -n ${EDGE_PID:-} ]] && kill "$EDGE_PID" 2>/dev/null; }
trap cleanup EXIT

rm -rf "$WORK"; mkdir -p "$WORK"

# --- a private feed with one package, at the production path layout -----------
# The private tree mirrors the public one, so objects live under
# <rel>/orgs/<org>/<branch>/target/<target>/. edge.py strips everything up to and
# including the branch, so the served root must still carry `target/<target>`.
FEEDROOT=$WORK/feed
REPODIR=$FEEDROOT/target/$TARGET
mkdir -p "$REPODIR"
command -v rpmbuild >/dev/null || die "rpmbuild is required to build the test package"
cat > "$WORK/hello-private.spec" <<'SPEC'
Name: hello-private
Version: 1.0
Release: 1
Summary: a package that only exists behind auth
License: MIT
BuildArch: noarch
%description
Proof that the CLI reached a feed it had to authenticate to.
%install
mkdir -p %{buildroot}/usr/share/hello-private
echo private > %{buildroot}/usr/share/hello-private/marker
%files
/usr/share/hello-private/marker
SPEC
rpmbuild --quiet -bb --define "_topdir $WORK/rpmbuild" "$WORK/hello-private.spec" >/dev/null 2>&1 \
  || die "rpmbuild failed"
cp "$WORK"/rpmbuild/RPMS/noarch/*.rpm "$REPODIR/"
command -v createrepo_c >/dev/null || die "createrepo_c is required"
createrepo_c --quiet "$REPODIR" || die "createrepo_c failed"
ok "built hello-private 1.0 into a private feed with repodata"

# --- the stand-in Connect + edge ---------------------------------------------
PORT=$(python3 -c "import socket;s=socket.socket();s.bind(('',0));print(s.getsockname()[1]);s.close()")
# --tier 3, deliberately not the default: it is what proves the User-Agent
# carries the tier the mint issued rather than a hard-coded floor.
python3 edge.py --dir "$FEEDROOT" --port "$PORT" --pat "$PAT" --org "$ORG" --tier 3 \
  --window 60 --limit-anon 1000 --limit-tier 1:5000 --limit-tier 3:5000 > "$WORK/edge.log" 2>&1 &
EDGE_PID=$!
for _ in $(seq 1 40); do
  curl -sf -o /dev/null "http://127.0.0.1:$PORT/.well-known/jwks.json" && break
  sleep 0.25
done
BASE="http://127.0.0.1:$PORT"
OBJ="private/$REL/orgs/$ORG/$BRANCH/target/$TARGET/repodata/repomd.xml"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/$OBJ")
[[ $code == 401 ]] && ok "the feed challenges anonymous access (401)" \
                   || bad "expected 401 from the feed, got $code"

# --- a project whose only feed is the private one -----------------------------
PROJ=$WORK/project
mkdir -p "$PROJ"
cat > "$PROJ/avocado.yaml" <<YAML
default_target: $TARGET
supported_targets: [$TARGET]

distro:
  release: $REL
  channel: next
  # The private feed first: it must be reachable, not merely configured.
  feeds: [private, avocado]

repos:
  private:
    org: $ORG
    release: $REL
    channel: $BRANCH
    gpgcheck: false

runtimes:
  dev:
    packages: {}

sdk:
  image: "docker.io/avocadolinux/sdk:2026"
  # Deliberately not --network=host: the minted URL is a loopback address, so the
  # rewrite and --add-host have to work on docker's default bridge, exactly as
  # they must for any locally hosted Connect.
YAML

# Env credentials rather than a stored profile: this is the CI path, and it keeps
# the test from touching the developer's real Connect config.
export AVOCADO_CONNECT_TOKEN=$PAT
export AVOCADO_CONNECT_URL=$BASE

# --- the CLI resolves, mints, and hands dnf a working repo --------------------
cd "$PROJ"
# The SDK has to exist before any dnf passthrough: dnf dies on a missing rpmrc
# inside the container before it makes a single HTTP request. This is the one
# network step, and it pulls from the public feed, not the private one.
echo "bootstrapping SDK (network) ..."
if "$AVOCADO" --no-tui sdk install --force > "$WORK/sdk-install.log" 2>&1; then
  ok "sdk install"
else
  tail -20 "$WORK/sdk-install.log" >&2
  bad "sdk install failed (see $WORK/sdk-install.log)"
fi

if "$AVOCADO" --no-tui sdk dnf --dnf-arg --nogpgcheck repoquery hello-private \
     > "$WORK/dnf.log" 2>&1; then
  grep -q "hello-private" "$WORK/dnf.log" \
    && ok "dnf resolved hello-private from the private feed" \
    || bad "dnf ran but did not find hello-private (see $WORK/dnf.log)"
else
  bad "the dnf passthrough failed (see $WORK/dnf.log)"
fi

# --- the canonical document must record the org, never the token --------------
DOC="$PROJ/.avocado/feeds/$TARGET.json"
if [[ -f $DOC ]]; then
  grep -q "connect:$ORG" "$DOC" && ok "canonical document records connect:$ORG" \
                                || bad "canonical document has no org identity"
  if grep -qE '"password"|eyJ[A-Za-z0-9_-]{10}' "$DOC"; then
    bad "canonical document contains a token — it must never be written down"
  else
    ok "canonical document carries no token"
  fi
else
  bad "no canonical document at $DOC"
fi

# --- the mint actually happened, and only once per invocation ----------------
# Exactly one mint per CLI invocation, and this rig makes two that touch feeds
# (`sdk install`, then the dnf passthrough). Not a tidiness check: before feeds
# were materialized once per invocation this was five mints for a single build,
# because every container run re-minted. A per-minute limit on the mint endpoint
# would then refuse an ordinary build.
MINTS=$(grep -c "POST /api/orgs/$ORG/feed-tokens status=200" "$WORK/edge.log" || true)
[[ ${MINTS:-0} -eq 2 ]] && ok "one feed token per invocation ($MINTS mints for 2 invocations)" \
                        || bad "expected 2 mints (one per invocation), got ${MINTS:-0}"
AUTHED=$(grep -c "GET /private/.* status=200" "$WORK/edge.log" || true)
[[ ${AUTHED:-0} -ge 1 ]] && ok "authenticated feed reads succeeded ($AUTHED)" \
                         || bad "no authenticated reads reached the feed"
UA=$(grep -oE "key=[0-9a-f]+" "$WORK/edge.log" | head -1)
[[ -n $UA ]] && ok "requests carried the client identity ($UA)" \
             || bad "no client identity in the feed's request log"

# The tier must be the one the mint issued, not a constant. edge.py is started
# with --tier 3, so a hard-coded tier/1 fails here. Without this the tier in the
# mint response is decorative and every authenticated client shares one
# rate-limit bucket whatever Connect assigned.
TIERS=$(grep -oE "tier=[0-9]+" "$WORK/edge.log" | sort -u | tr '\n' ' ')
if grep -qE "GET /private/.* tier=3" "$WORK/edge.log"; then
  ok "feed requests carried the minted tier (saw: $TIERS)"
else
  bad "feed requests did not carry the minted tier 3 (saw: $TIERS)"
fi

echo
if [[ $FAIL -eq 0 ]]; then
  echo "ALL PASSED ($PASS assertions)  workdir: $WORK"
else
  echo "$FAIL FAILED, $PASS passed  workdir: $WORK"
fi
exit $(( FAIL > 0 ))
