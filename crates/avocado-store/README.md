# avocado-store (PoC)

A content-addressed layer store for Avocado build state, in the composefs model:
every file's bytes are stored once by sha256, a layer is a *complete tree* described by
metadata only, and a project's `/opt/_avocado/<target>` is an overlay **view** of one layer
plus a private upper. Layers are immutable once committed, shared across projects and
targets, and move between machines over the OCI distribution protocol, fetching only the
objects the receiver lacks.

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

## Sync (OCI)

`push <http://host:port/repo> K [--known P…]` / `pull <registry> K`. A layer is an image index
tagged K: manifest 0 has the layer record as config and `[erofs, gzipped dump]` as layers;
manifests 1… list the layer's objects (1000 per manifest) so registry GC sees them referenced.
Push skips objects of `--known` layers the remote already has; pull fetches the image, the dump,
and only the objects missing locally, verifying every digest. Any OCI 1.1 registry works (PoC: zot).

`advertise <registry> <name> --extra '{…}'` publishes this store's descriptor (host, cpus, memory,
free space, warm layers + their inputs) as tag `backend-<name>`; `plan <topology.json>
--registry …` merges advertised backends into a topology and prints a build/route plan.

## Cache management

`gc --high H --low L [--dry-run]`: when the store (objects + layer files) is over H bytes,
evict least-recently-used layers (record mtime, touched on every mount/use) that no pin and
no view references, until it is at or under L; then delete objects no remaining layer
references. Unreferenced objects (e.g. from a crashed writer) go first. Records are deleted
before their files and objects last, all under the exclusive GC lock. Layers are complete
trees, so keeping a layer never requires keeping its parent.

## Commands

```
preflight  import <dir> <key>  has  record  mount  umount  drop  use  commit  remount  alias
verify  stats  describe  advertise  plan  gc  pin  unpin  hold  push  pull  checkout  destroy
```

`hold <key>` takes the per-key build lock and keeps it until its stdin closes; the CLI keeps
that pipe open for the duration of a step, so a killed CLI releases the lock.

## Requirements

Rootful docker on Linux >= 6.6 (data-only overlay lowers; 6.12+ for file-backed EROFS), or
avocado-vm. `preflight` mounts a real view and says "use avocado-vm" otherwise.
Build: `cargo build --release --target x86_64-unknown-linux-musl`, then
`docker build -f Dockerfile.helper` with the binary in the context
(`~/hub-poc/layer-store/bench/helper-image.sh` does both on any DOCKER_HOST).
