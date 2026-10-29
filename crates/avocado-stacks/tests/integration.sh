#!/bin/bash
# Run the stacks integration tests (#[ignore]d, real mounts) as root inside the helper image.
# Needs: docker with --privileged, kernel 6.12+, and a scratch directory on a filesystem that can
# hold an overlay upper (ext4, xfs or btrfs; not tmpfs). Usage:
#   tests/integration.sh /var/tmp/avocado-stacks-it
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
scratch=${1:?scratch directory on a real filesystem}
image=${AVOCADO_STACKS_HELPER:-avocado-stacks:poc}
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
docker run --rm --privileged \
  -v "$bin:/it-tests:ro" -v "$scratch:$scratch:rshared" \
  -e AVOCADO_STACKS_IT_DIR="$scratch" --entrypoint /it-tests "$image" \
  --ignored --test-threads=1
