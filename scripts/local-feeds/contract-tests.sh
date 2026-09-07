#!/usr/bin/env bash
# The feed edge contract, as executable assertions.
#
#   contract-tests.sh --base-url URL --pat TOKEN --org NAME [--path OBJECT]
#
# Runs against ANY implementation of `edge-contract.md`: the local adaptor
# (edge.py), or a real staging distribution once CloudFront, Lambda@Edge and
# Connect are in place. That is the whole point — the assertions below name no
# implementation, only observable behaviour, so a divergence in production shows
# up here as a named failing row rather than as an incident.
#
# Covers S2 (authorization), S3 (token issuance) and S4 (rate limiting).
# S5 (caching) and S6 (attribution) are not covered yet; they have no client
# consumer, and the contract document records why they wait.
set -uo pipefail

BASE_URL=""; PAT="test-pat"; ORG="acme"; OBJ="repodata/repomd.xml"
REL="2026"; BRANCH="main"
while [[ $# -gt 0 ]]; do
  case $1 in
    --base-url) BASE_URL=$2; shift 2 ;;
    --pat) PAT=$2; shift 2 ;;
    --org) ORG=$2; shift 2 ;;
    --path) OBJ=$2; shift 2 ;;
    --release) REL=$2; shift 2 ;;
    --branch) BRANCH=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[[ -n $BASE_URL ]] || { echo "--base-url is required" >&2; exit 2; }

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf 'FAIL %s\n' "$*"; }
check() { # check <description> <expected> <actual>
  if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1 (expected $2, got $3)"; fi
}

ua() { printf 'avocado-cli/1.0.0-test;key/%s;tier/%s' "$1" "$2"; }
status() { # status <ua> <curl args...>
  local u=$1; shift
  curl -s -o /dev/null -w '%{http_code}' -A "$u" "$@"
}
mint() { # mint <json-body> -> response body
  curl -s -X POST -H "Authorization: Bearer $PAT" -H 'Content-Type: application/json' \
    -d "$1" "$BASE_URL/api/orgs/$ORG/feed-tokens"
}
jfield() { python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('$1',''))"; }

# The private tree mirrors the public one: the release precedes the org, so the
# org is a path SEGMENT rather than a prefix. Getting this wrong is how a scope
# check silently degrades into "any authenticated org can read any org".
FEED="$BASE_URL/private/$REL/orgs/$ORG/$BRANCH/$OBJ"

echo "--- S3  token issuance"
BODY=$(mint '{}')
TOKEN=$(printf '%s' "$BODY" | jfield token)
TIER=$(printf '%s' "$BODY" | jfield tier)
[[ -n $TOKEN ]] && ok "mint returns a token" || bad "mint returned no token: $BODY"
[[ -n $TIER  ]] && ok "mint returns the tier (contract, not decoration): tier=$TIER" \
                || bad "mint returned no tier"
# The tier is an entitlement, not a request field. A client that could ask for a
# tier could ask for a higher one; it buys only a rate-limit bucket, but it is
# still a privilege taken from client input.
ASKED=$(mint '{"tier": 99}' | jfield tier)
check "a client-requested tier is ignored" "$TIER" "$ASKED"
check "a bad account credential is refused" 403 \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Authorization: Bearer wrong-pat' \
      -d '{}' "$BASE_URL/api/orgs/$ORG/feed-tokens")"

echo "--- S2  request authorization"
# 401 with a Basic challenge, not 403: dnf authenticates only when challenged,
# so a 403 here fails every fetch outright and a Bearer challenge is ignored.
check "no credential is challenged, not refused" 401 "$(status "$(ua k1 1)" "$FEED")"
WWW=$(curl -s -D- -o /dev/null -A "$(ua k1 1)" "$FEED" | tr -d '\r' \
      | awk 'tolower($1)=="www-authenticate:"{print tolower($2)}')
check "the challenge says Basic"            basic "$WWW"
check "a valid token is served"             200 "$(status "$(ua k1 1)" -u "k1:$TOKEN" "$FEED")"
check "a garbage token is refused"          403 "$(status "$(ua k1 1)" -u "k1:not-a-token" "$FEED")"

SHORT=$(mint '{"ttl": 1}' | jfield token)
sleep 2
check "an expired token is refused"         403 "$(status "$(ua k1 1)" -u "k1:$SHORT" "$FEED")"

check "another org's path is refused with a valid token" 403 \
  "$(status "$(ua k1 1)" -u "k1:$TOKEN" "$BASE_URL/private/$REL/orgs/someone-else/$BRANCH/$OBJ")"

ROTATED=$(mint '{"previous_key": true}' | jfield token)
check "a token signed with the previous key still works (rotation safety)" 200 \
  "$(status "$(ua k1 1)" -u "k1:$ROTATED" "$FEED")"

echo "--- S4  rate limiting"
# Drive one client past its ceiling. The limit is unknown to this suite by
# design: the contract is the 429, not the number, so the loop stops on the
# first one and fails only if the ceiling is never reached.
LIMITED=""; TRIES=0
for _ in $(seq 1 60); do
  TRIES=$((TRIES+1))
  code=$(status "$(ua burst 0)" -u "burst:$TOKEN" "$FEED")
  [[ $code == 429 ]] && { LIMITED=yes; break; }
done
[[ -n $LIMITED ]] && ok "an anonymous-tier client is limited (after $TRIES requests)" \
                  || bad "no 429 after $TRIES requests — the ceiling was never reached"

if [[ -n $LIMITED ]]; then
  RA=$(curl -s -D- -o /dev/null -A "$(ua burst 0)" -u "burst:$TOKEN" "$FEED" \
        | tr -d '\r' | awk 'tolower($1)=="retry-after:"{print $2}')
  [[ -n $RA ]] && ok "the 429 carries Retry-After: $RA" \
               || bad "the 429 carries no Retry-After — dnf sees an unexplained failure"
fi

# The property that proves counters are per client rather than global.
check "a second client at the same tier is unaffected" 200 \
  "$(status "$(ua quiet 0)" -u "quiet:$TOKEN" "$FEED")"

# And that the tier actually selects a different ceiling. A higher tier must
# survive a burst that limited tier 0.
HIGH_OK=yes
for _ in $(seq 1 $TRIES); do
  code=$(status "$(ua highburst 1)" -u "highburst:$TOKEN" "$FEED")
  [[ $code == 429 ]] && { HIGH_OK=""; break; }
done
[[ -n $HIGH_OK ]] && ok "a higher tier survives the burst that limited tier 0" \
                  || bad "tier 1 was limited at the same point as tier 0 — tiers are not selecting a ceiling"

echo
printf '%s\n' "-------------------------------------------"
if [[ $FAIL -eq 0 ]]; then
  echo "ALL PASSED ($PASS assertions) against $BASE_URL"
else
  echo "$FAIL FAILED, $PASS passed against $BASE_URL"
fi
exit $(( FAIL > 0 ))
