#!/bin/bash
# End-to-end check of stacks against real feeds and docker.
#
# Needs: Linux 6.12+, rootful docker that can run --privileged, the helper image built from
# crates/avocado-stacks (see its README), network access to the feeds, and a checkout of
# https://github.com/avocado-linux/references. Takes 20-30 minutes; cleans up after itself.
#
#   AVOCADO=target/release/avocado REFS=~/src/references scripts/stacks-e2e.sh
#
# AVOCADO  the CLI under test (required)
# REFS     references checkout (required)
# WORK     scratch directory on ext4, xfs or btrfs (default /var/tmp/avocado-stacks-e2e)
# HELPER   helper image (default avocado-stacks:dev, built from crates/avocado-stacks/Dockerfile.helper)
set -uo pipefail
AVO=$(realpath "${AVOCADO:?set AVOCADO to the avocado binary under test}")
REFS=$(realpath "${REFS:?set REFS to a references checkout}")
WORK=${WORK:-/var/tmp/avocado-stacks-e2e}
HELPER=${HELPER:-avocado-stacks:dev}
export AVOCADO_STACKS=$WORK/stacks AVOCADO_STACKS_HELPER=$HELPER
P=$WORK/projects L=$WORK/logs
mkdir -p "$AVOCADO_STACKS" "$P" "$L"
pass=0 fail=0
check() { if eval "$2"; then echo "PASS: $1"; pass=$((pass + 1)); else echo "FAIL: $1"; fail=$((fail + 1)); fi; }
inst() { (cd "$1" && "$AVO" sdk install --no-tui "${@:3}") >"$2" 2>&1; }
count() { grep -c "$1" "$2"; }
inview() { count 'already in the view' "$1"; }
hits() { count 'Stacks: .* hit ' "$1"; }
built() { count 'Stacks: .* committed [0-9a-f]' "$1"; }
peek() { (cd "$1" && "$AVO" sdk run --no-tui -- bash -lc "echo VAL:\$($2)") 2>/dev/null | sed -n 's/^VAL://p' | tail -1; }
lock_has() { python3 - "$1/avocado.lock" "${@:2}" <<'PY'
import json, sys
t = next(iter(json.load(open(sys.argv[1]))["targets"].values()))
sys.exit(0 if all(t.get(k) for k in sys.argv[2:]) else 1)
PY
}
volume_of() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['volume_name'])" "$1/.avocado-state"; }
views() { "$AVO" stack status --json | python3 -c "import json,sys;print(len(json.load(sys.stdin)['views']))"; }
layers() { "$AVO" stack status --json | python3 -c "import json,sys;print(len(json.load(sys.stdin)['layers']))"; }
# A project's SDK tree as data, private paths and build stamps left out: every entry's type,
# path, mode and owner (and size and link target for non-directories), every file's sha256, and
# every rpm database's packages with their checksums. Two installs with the same inputs must
# give the same manifest, however their layers came about. Left out as bytes: dnf's logs, cache
# and history, and the rpm database's SQLite files, which differ between any two installs of the
# same packages (the package list stands for the database).
MANIFEST='cd "$AVOCADO_PREFIX" || exit 1
set -- \( -path ./includes -o -path ./extensions -o -path ./runtimes -o -path ./output \
  -o -path ./kernel -o -path ./var-staging -o -path ./.stamps -o -path "*/var/log" \
  -o -path "*/var/cache" -o -path "*/var/lib/dnf" -o -name "rpmdb.sqlite*" \) -prune -o
find . -xdev "$@" -type d -printf "E d %p %m %U:%G\n" -o "$@" -printf "E %y %p %m %U:%G %s %l\n"
find . -xdev "$@" -type f -print0 | xargs -0 -r sha256sum | sed "s/^/H /"
for d in $(find . -xdev \( -path ./includes -o -path ./extensions -o -path ./runtimes \
  -o -path ./output -o -path ./kernel -o -path ./var-staging \) -prune -o -name rpmdb.sqlite -printf "%h\n"); do
  rpm --dbpath "$PWD/${d#./}" -qa --qf "P $d %{NEVRA} %{SIGMD5}\n"
done'
manifest() { (cd "$1" && "$AVO" sdk run --no-tui -- bash -c "$MANIFEST") 2>/dev/null | grep -E '^(E|H|P) ' | sort >"$2"; }
# same_tree <project> <project> <name>: equal manifests (the diff's head goes to the output).
same_tree() {
  manifest "$1" "$L/$3.left"
  manifest "$2" "$L/$3.right"
  # Hundreds of packages across the sysroots: a manifest without them compared nothing.
  [ "$(wc -l <"$L/$3.left")" -gt 1000 ] && [ "$(grep -c '^P ' "$L/$3.left")" -gt 100 ] &&
    diff -q "$L/$3.left" "$L/$3.right" >/dev/null && return 0
  echo "  $(wc -l <"$L/$3.left") vs $(wc -l <"$L/$3.right") entries; first differences:"
  diff "$L/$3.left" "$L/$3.right" | head -20 | sed 's/^/  /'
  return 1
}
# Entries under a volume's view mount point on the volume itself (what the view hides).
hidden_under() {
  local vol=$1 data
  data=$(docker volume inspect -f '{{.Mountpoint}}' "$vol")
  docker run --rm --entrypoint ls \
    --mount "type=bind,src=$data,dst=/v,readonly,bind-recursive=disabled" "$HELPER" -A "/v/$2" ||
    echo "missing: /v/$2"
}
hidden_count() { hidden_under "$@" | wc -l
}
marker() { peek "$1" 'cat $AVOCADO_PREFIX/rootfs/etc/e2e-marker'; }

teardown() {
  local R=$AVOCADO_STACKS
  docker run --rm --privileged -v "$R:$R:rshared" -v /var/lib/docker/volumes:/var/lib/docker/volumes:rshared \
    "$HELPER" --root "$R" destroy >/dev/null 2>&1
  for v in $(docker volume ls -q --filter label=avocado.source_path | grep '^avo-'); do
    case "$(docker volume inspect -f '{{index .Labels "avocado.source_path"}}' "$v")" in
      "$P"/*) docker volume rm -f "$v" >/dev/null ;;
    esac
  done
  docker run --rm -v "$WORK:/w" --entrypoint rm "$HELPER" -rf /w/stacks
  rm -rf "$P"
  echo "teardown: $(grep -c " $WORK/" /proc/self/mountinfo) mounts left"
}
trap teardown EXIT

# mk <name> <reference> [overlay marker]
mk() {
  rm -rf "${P:?}/$1"
  cp -r "$REFS/$2" "$P/$1"
  rm -f "$P/$1/.avocado-state"
  if [ -n "${3:-}" ]; then
    mkdir -p "$P/$1/rootfs-overlay/etc"
    echo "$3" >"$P/$1/rootfs-overlay/etc/e2e-marker"
    sed -i 's/^rootfs:$/rootfs:\n  overlay: rootfs-overlay/' "$P/$1/avocado.yaml"
  fi
}
mk a shell-heartbeat marker-A
mk b shell-heartbeat marker-B
mk c shell-heartbeat marker-A
mk d cpp-tui-dashboard

echo "== A: cold, warm, build"
inst "$P/a" "$L/a-cold.log"
check "A cold builds 3 layers" '[ $(built "$L/a-cold.log") -eq 3 ]'
check "A's lock has sdk, rootfs and initramfs pins" 'lock_has "$P/a" sdk rootfs initramfs'
check "A's rootfs has its overlay" '[ "$(peek "$P/a" "cat \$AVOCADO_PREFIX/rootfs/etc/e2e-marker")" = marker-A ]'
peek "$P/a" 'mkdir -p $AVOCADO_PREFIX/.stamps/ext/e2e && echo kept > $AVOCADO_PREFIX/.stamps/ext/e2e/marker && echo ok' >/dev/null
inst "$P/a" "$L/a-warm.log"
check "A warm: every step already in the view, nothing switched or built" \
  '[ $(inview "$L/a-warm.log") -eq 3 ] && [ $(hits "$L/a-warm.log") -eq 0 ] && [ $(built "$L/a-warm.log") -eq 0 ]'
check "A warm: project build stamps survive" '[ "$(peek "$P/a" "cat \$AVOCADO_PREFIX/.stamps/ext/e2e/marker")" = kept ]'
(cd "$P/a" && "$AVO" install --no-tui && "$AVO" build --no-tui) >"$L/a-build.log" 2>&1; rc=$?
check "A: install and build on the view" '[ $rc -eq 0 ]'

echo "== A: unlock --rootfs, reinstall (pins come back from the layer)"
for i in 1 2; do
  (cd "$P/a" && "$AVO" unlock --rootfs) >/dev/null 2>&1
  inst "$P/a" "$L/a-unlock-$i.log"; rc=$?
  check "A unlock round $i: install succeeds and rootfs pins are back" '[ $rc -eq 0 ] && lock_has "$P/a" rootfs'
done

echo "== C = A's inputs: shares everything"
inst "$P/c" "$L/c.log"
check "C hits every layer, builds none" '[ $(built "$L/c.log") -eq 0 ] && [ $(( $(hits "$L/c.log") + $(inview "$L/c.log") )) -eq 3 ]'
check "C's lock got the pins of what it reused" 'lock_has "$P/c" sdk rootfs initramfs'

echo "== B = A with another overlay file"
inst "$P/b" "$L/b.log"
check "B shares the SDK bootstrap and builds its own base" '[ $(hits "$L/b.log") -ge 1 ] && grep -q "L1 target base committed" "$L/b.log"'
check "B's rootfs has B's overlay, not A's" '[ "$(peek "$P/b" "cat \$AVOCADO_PREFIX/rootfs/etc/e2e-marker")" = marker-B ]'

echo "== the same inputs give the same tree, whichever way it was built"
mk aplain shell-heartbeat marker-A
mk bplain shell-heartbeat marker-B
AVOCADO_STACKS=off inst "$P/aplain" "$L/aplain.log"; rc=$?
check "A's inputs install on plain volumes" '[ $rc -eq 0 ] && ! grep -q "Stacks:" "$L/aplain.log"'
check "A, built cold on stacks, matches the plain install" 'same_tree "$P/a" "$P/aplain" a-vs-plain'
check "C, built only from A's stored layers, matches the plain install" 'same_tree "$P/c" "$P/aplain" c-vs-plain'
AVOCADO_STACKS=off inst "$P/bplain" "$L/bplain.log"
check "B, which shares A's bootstrap layer, matches its own plain install" 'same_tree "$P/b" "$P/bplain" b-vs-plain'

echo "== D: a project with a compile section"
inst "$P/d" "$L/d-cold.log"; rc=$?
check "D builds its project layer with the compile dependencies" '[ $rc -eq 0 ] && grep -q "L2 project committed" "$L/d-cold.log" && lock_has "$P/d" target-sysroot'
inst "$P/d" "$L/d-warm.log"
check "D warm: every step already in the view" '[ $(inview "$L/d-warm.log") -eq 3 ] && [ $(built "$L/d-warm.log") -eq 0 ]'

echo "== parallel installs"
mk p1 shell-heartbeat marker-P1
mk p2 shell-heartbeat marker-P2
inst "$P/p1" "$L/p1.log" & j1=$!
inst "$P/p2" "$L/p2.log" & j2=$!
wait $j1; r1=$?; wait $j2; r2=$?
check "two projects that differ in one input install at the same time" '[ $r1 -eq 0 ] && [ $r2 -eq 0 ]'
check "each has its own overlay" '[ "$(marker "$P/p1")" = marker-P1 ] && [ "$(marker "$P/p2")" = marker-P2 ]'
mk q1 shell-heartbeat marker-Q
mk q2 shell-heartbeat marker-Q
inst "$P/q1" "$L/q1.log" & j1=$!
inst "$P/q2" "$L/q2.log" & j2=$!
wait $j1; r1=$?; wait $j2; r2=$?
check "two projects with the same inputs at the same time: one builds the base, the other reuses it" \
  '[ $r1 -eq 0 ] && [ $r2 -eq 0 ] && [ $(cat "$L/q1.log" "$L/q2.log" | grep -c "L1 target base committed") -eq 1 ] && [ "$(marker "$P/q1")$(marker "$P/q2")" = marker-Qmarker-Q ]'

echo "== commit: false"
before=$(layers)
mk n shell-heartbeat marker-N
AVOCADO_STACKS_COMMIT=0 inst "$P/n" "$L/n.log"; rc=$?
check "an install that may not commit still gets its own tree" '[ $rc -eq 0 ] && [ "$(marker "$P/n")" = marker-N ]'
check "and adds no layer" '[ $(built "$L/n.log") -eq 0 ] && grep -q "not committed" "$L/n.log" && [ $(layers) -le $before ]'
mk n2 shell-heartbeat marker-A
AVOCADO_STACKS_COMMIT=0 inst "$P/n2" "$L/n2.log"; rc=$?
check "it still reuses committed layers" '[ $rc -eq 0 ] && [ $(built "$L/n2.log") -eq 0 ] && [ $(( $(hits "$L/n2.log") + $(inview "$L/n2.log") )) -eq 3 ]'

echo "== a project installed before stacks"
mk m shell-heartbeat marker-A
AVOCADO_STACKS=off inst "$P/m" "$L/m-plain.log"
AVOCADO_STACKS=off peek "$P/m" 'mkdir -p $AVOCADO_PREFIX/runtimes/e2e && echo kept >$AVOCADO_PREFIX/runtimes/e2e/out && echo ok' >/dev/null
inst "$P/m" "$L/m-stacks.log"; rc=$?
check "switching it to stacks reuses the stored layers" '[ $rc -eq 0 ] && [ $(built "$L/m-stacks.log") -eq 0 ]'
check "its project outputs survive the switch" '[ "$(peek "$P/m" "cat \$AVOCADO_PREFIX/runtimes/e2e/out")" = kept ]'
check "its tree matches the plain install" 'same_tree "$P/m" "$P/aplain" m-vs-plain'
check "the plain install is gone from under the view, not hidden" '[ "$(hidden_count "$(volume_of "$P/m")" qemux86-64)" -eq 0 ] && grep -q "took over this project" "$L/m-stacks.log"'

echo "== a busy project"
(cd "$P/a" && "$AVO" sdk run --no-tui -- sleep 60) >"$L/a-busy-run.log" 2>&1 &
busy=$!
sleep 15
inst "$P/a" "$L/a-busy.log" --dnf-arg=--setopt=keepcache=False; rc=$?
check "an install that must switch the view refuses while another command uses it" \
  '[ $rc -ne 0 ] && grep -q "container(s) are using this project" "$L/a-busy.log"'
kill $busy 2>/dev/null
wait $busy 2>/dev/null
docker ps -q --filter "volume=$(volume_of "$P/a")" | xargs -r docker rm -f >/dev/null

echo "== visibility"
"$AVO" stack status >"$L/status.txt" 2>&1
cat "$L/status.txt"
nviews=$(views)
check "status lists every project with a view by absolute path" '[ $nviews -ge 10 ] && [ $(grep -c "^  $P/" "$L/status.txt") -eq $nviews ]'
check "status --json parses" '"$AVO" stack status --json | python3 -c "import json,sys;json.load(sys.stdin)"'

echo "== reclaiming"
rm -rf "${P:?}/c"
"$AVO" stack prune --dry-run >"$L/prune-dry.log" 2>&1
check "prune --dry-run names C's view and changes nothing" 'grep -q "would drop" "$L/prune-dry.log" && [ $(views) -eq $nviews ]'
"$AVO" stack prune >"$L/prune.log" 2>&1
check "prune drops only C's view" '[ $(views) -eq $((nviews - 1)) ]'
volA=$(volume_of "$P/a")
(cd "$P/a" && "$AVO" clean) >"$L/a-clean.log" 2>&1; rc=$?
check "clean succeeds with stacks" '[ $rc -eq 0 ]'
check "clean removed A's volume and view" '! docker volume inspect "$volA" >/dev/null 2>&1 && ! "$AVO" stack status --json | grep -q "$volA"'
check "no mounts left inside A's volume" '! grep -q "volumes/$volA/" /proc/self/mountinfo'
"$AVO" stack gc --dry-run >"$L/gc-dry.log" 2>&1
"$AVO" stack gc >"$L/gc.log" 2>&1
check "gc reclaims everything no project holds" \
  '[ "$("$AVO" stack status --json | python3 -c "import json,sys;print(json.load(sys.stdin)[\"reclaimable_bytes\"])")" -eq 0 ]'
inst "$P/b" "$L/b-after-gc.log"
check "B still installs from its layers after gc" '[ $(built "$L/b-after-gc.log") -eq 0 ]'

echo "== configured stacks whose helper cannot run"
mk e shell-heartbeat
(cd "$P/e" && AVOCADO_STACKS_HELPER=avocado-stacks:does-not-exist "$AVO" sdk install --no-tui) >"$L/e.log" 2>&1; rc=$?
check "install falls back to plain volumes with a warning" '[ $rc -eq 0 ] && grep -q "Using plain volumes" "$L/e.log" && ! grep -q "Stacks: .* committed" "$L/e.log"'
(cd "$P/e" && AVOCADO_STACKS_HELPER=avocado-stacks:does-not-exist "$AVO" clean) >"$L/e-clean.log" 2>&1; rc=$?
check "clean still works there" '[ $rc -eq 0 ]'
(cd "$P/b" && AVOCADO_STACKS_HELPER=avocado-stacks:does-not-exist "$AVO" sdk install --no-tui) >"$L/b-no-helper.log" 2>&1; rc=$?
check "a project whose SDK lives in stacks refuses a plain install while stacks cannot run" \
  '[ $rc -ne 0 ] && grep -q "lives in stacks" "$L/b-no-helper.log"'

echo "== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
