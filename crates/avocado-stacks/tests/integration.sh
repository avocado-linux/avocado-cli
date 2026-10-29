#!/bin/bash
# Run the stacks integration tests (#[ignore]d, real mounts) as root inside the helper image.
# Needs: docker with --privileged, kernel 6.5+, the helper image (default avocado-stacks:dev,
# built from Dockerfile.helper), and a scratch directory on a filesystem that can hold an overlay
# upper (ext4, xfs or btrfs; not tmpfs). Usage:
#   tests/integration.sh /var/tmp/avocado-stacks-it
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
scratch=${1:?scratch directory on a real filesystem}
image=${AVOCADO_STACKS_HELPER:-avocado-stacks:dev}
mkdir -p "$scratch"
bin=$(cd "$here" && cargo test --release --lib --no-run --message-format=json 2>/dev/null |
  python3 -c 'import json,sys
for l in sys.stdin:
    m = json.loads(l)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["kind"] == ["lib"]:
        print(m["executable"])')
# A failed test panics before its own cleanup: unmount and remove whatever it left, always.
cleanup() {
  docker run --rm --privileged -v "$scratch:$scratch:rshared" --entrypoint sh "$image" -c \
    "awk '\$5 ~ \"^$scratch/it-\" {print \$5}' /proc/self/mountinfo | sort -r | xargs -r umount -l; rm -rf $scratch/it-*"
}
trap cleanup EXIT
# Twice: the way this kernel mounts EROFS (file-backed on 6.12+), then forced through a loop
# device (the 6.5-6.11 path). /dev is the host's, as the CLI gives the helper, so loop nodes the
# kernel creates on demand are visible.
for force_loop in 0 1; do
  echo "== AVOCADO_STACKS_FORCE_LOOP=$force_loop"
  docker run --rm --privileged -v /dev:/dev \
    -v "$bin:/it-tests:ro" -v "$scratch:$scratch:rshared" \
    -e AVOCADO_STACKS_IT_DIR="$scratch" -e AVOCADO_STACKS_FORCE_LOOP=$force_loop \
    --entrypoint /it-tests "$image" --ignored --test-threads=1
done
