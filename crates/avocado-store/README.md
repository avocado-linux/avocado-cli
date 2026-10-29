# avocado-store (PoC)

A content-addressed layer store for Avocado build state, in the composefs model:
every file's bytes are stored once by sha256, a layer is a *complete tree* described by
metadata only, and a project's `/opt/_avocado/<target>` is an overlay **view** of one layer
plus a private upper. Layers are immutable once committed and shared across projects on one
store. Moving layers between machines (an OCI registry, fetching only the objects the receiver
lacks) is a later step: it needs signed layer records, TLS and auth first.

This crate is the store engine. It runs as root inside a short-lived privileged helper
container (`avocado-store:poc`, see `Dockerfile.helper`), so native Linux hosts and
avocado-vm take the same code path. The CLI side is `src/utils/layer_store.rs`.

## On-disk format

```
<root>/
  objects/ab/cdef…            file contents, named by sha256 (the only copy of any file)
  layers/<key>.dump           canonical composefs dump: the full tree
  layers/<key>.erofs          that dump compiled by `mkcomposefs --from-file`
  layers/<key>.json           the layer record; its presence is the commit point
  views/<name>.json           a view: {layer, private, at}; its existence is a lease on the layer
  views/<name>/{upper,work}   the view's private upper (and `merged/` unless mounted `at` elsewhere)
  locks/<key>.lock            per-key build lock (flock)
  pins/<name>                 newline-separated layer keys protected from GC
  gc.lock                     writers and mounts hold it shared, GC holds it exclusive
  tmp/                        staging (temp names are random per helper: every helper is PID 1)
```

- **Dump**: one line per entry, `composefs-dump(5)` format. Regular files point at
  `ab/cdef…` (relative to `objects/`); files of 64 bytes or less are inlined. Lines are in
  canonical order (depth-first, siblings sorted by name), `nlink` is not modelled, hardlinks are
  not modelled (each link is a file pointing at the same object: overlay would show them as
  separate inodes anyway), `security.selinux` and `trusted.overlay.*` xattrs are dropped.
- **Tree digest** = sha256 of the dump. `verify` mounts the layer and re-hashes every file
  through the view; equal digests mean the view is bit-identical to what was imported.
- **Layer key** = chosen by the caller. The CLI uses sha256 of `{parent, inputs}`: the step's
  parent layer plus its complete inputs (see the CLI notes). The record also stores `parent`,
  `tree_digest`, the inputs, counts and timings.
- **Aliases**: `alias A K` symlinks A's three files to K's (a second name for the same layer).

## Views

`mount <view> [--layer K] [--at PATH] [--private NAME…]` mounts:

```
erofs  layers/K.erofs  (mount(2), file-backed; staged in tmp/ and detached once the overlay holds it)
overlay lowerdir=<erofs>::<objects>,upperdir=…/upper,workdir=…/work,metacopy=on,redirect_dir=on
```

`objects/` is a data-only lower: the EROFS image's redirect xattrs point each file at its object.
Private names (top level: `includes`, `extensions`, `runtimes`, `output`, …) never enter a layer;
they stay in the upper across `use` and `commit`. That is also where secrets live (confext
claim tokens, signed images), so shared layers can't carry them.

- `use <view> K` switches a view to K, keeping only its private paths.
- `commit <view> K` harvests the mounted view into layer K (parent = the view's layer): walk the
  merged tree; a regular file is re-hashed only when the upper holds real data for it (or an
  ancestor dir was renamed); otherwise its object is reused from the parent's dump. Files from the
  upper are *moved* into `objects/` (same filesystem), then the view is remounted on K.
- `remount <prefix>` mounts every recorded, unmounted view with that name prefix (after a reboot).

## Private paths

`--private` entries are paths relative to the view root that never enter a layer and survive
`use`/`commit` in the view's upper: `includes`, `.stamps/ext`, or `dir/*` for every directory
directly under `dir` (`.stamps/rootfs/*`: the per-runtime image stamps, not the install stamp
beside them).

## Space and cleanup

`report` is the accounting the CLI's `avocado store status` renders: totals (objects, layer
metadata, private uppers, free space), what `gc` could reclaim now, every layer with its
`unique_bytes` (what evicting it alone frees: its metadata plus objects no other layer
references), its `logical_bytes`, its aliases, children and `protected_by` (`view <name>`,
`pin <name>`, `base`), and every view with its owner metadata (project, target, volume), upper
and private bytes. Sizes are bytes on disk (allocated blocks), so they add up to what a delete
frees.

`gc --high H --low L [--dry-run]` (or `--min-free F [--target-free T]`): when the store (objects
+ layer files) is over H bytes, evict layers no pin and no view holds until it is at or under L,
then delete objects no remaining layer references. Order: leaves before the layers they were
built on (a shared base frees little while its children exist, and every project needs it
again), least recently used first among equals (record mtime, touched on every mount and use).
Unreferenced objects (e.g. from a crashed writer) go first. An alias goes with its target and is
never counted as a layer of its own. Records are deleted before their files and objects last,
all under the exclusive GC lock. Layers are complete trees, so keeping a layer never requires
keeping its parent.

`drop <view>` / `drop --prefix <prefix>` releases views (their uppers and their hold on layers):
the CLI drops a project's views before its volume is removed (`avocado clean`, `prune`, the
volume reaper) and for orphans (`avocado store prune`).

## Integrity

Every dump read is validated: rooted, canonical paths (no empty, `.` or `..` component), object
references `ab/` + 62 lowercase hex digits, hardlink targets rooted, and no `trusted.overlay.*`
or `security.selinux` xattrs. A layer record (the commit point) is written only after a
`syncfs`, so a record never names files a power loss truncated. Store directories are 0700:
uppers hold project-private files and objects hold every project's files.

## Commands

```
preflight  import <dir> <key>  has  check  record  mount  umount  drop  use  commit  remount
alias  verify  report  gc  pin  unpin  hold  destroy
```

`hold <key>` takes the per-key build lock and keeps it until its stdin closes; the CLI keeps
that pipe open for the duration of a step, so a killed CLI releases the lock. Commands that
change a view (mount, use, commit, umount, drop) also hold a per-view lock.

## Tests

`cargo test`: unit tests (dump validation, private paths, GC order and alias handling, space
accounting); needs `mkcomposefs` on PATH. `tests/integration.sh <scratch dir>`: the `#[ignore]`d
mount tests (commit/use round trip, renames, failed-commit recovery, busy views) as root in the
helper image; needs privileged docker, kernel 6.12+ and a scratch directory on ext4, xfs or btrfs.
`scripts/layer-store-e2e.sh` (repo root) drives the CLI end to end against real feeds: shared
and unshared layers, warm re-runs, `unlock`, a compile section, a busy project, `status`,
`prune`, `clean`, `gc`, and a host whose helper cannot run.

## Requirements

Rootful docker on Linux 6.12+ (file-backed EROFS; data-only overlay lowers need 6.5+, and
6.5-6.11 would need a loop-device fallback that is not implemented yet), or avocado-vm.
`preflight` mounts a real view; the CLI runs it once per process and falls back to plain
volumes (with a warning) when it fails.
Build: `cargo build --release --target x86_64-unknown-linux-musl`, then
`docker build -f Dockerfile.helper -t avocado-store:poc` with the binary in the context.
