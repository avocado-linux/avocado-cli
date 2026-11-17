# Layer sharing

`sdk install` builds the SDK as content-addressed layers (the Stacks feature).
A layer built by one project is reused by another project whose inputs match.

## The rule

Layers are shared across projects when both hold:

1. The distro feed is pinned to a snapshot.
2. Every custom feed in `repos:` that is enabled has a content identity. A
   `repos:` feed counts only when it is enabled in `distro.feeds`.

Today only a `path:` feed has a content identity: the sha256 of its
`repodata/repomd.xml`. A `url:` or `org:` feed has none.

## Unpinned feeds keep layers private

A project that enables a `url:` or `org:` feed, or whose distro feed is not
pinned to a snapshot, keeps its layers private. The install prints a notice
giving the reason: the distro feed is not pinned, the first enabled feed with
no content identity (named), or both. Nothing is lost: the project builds and
uses its own layers, they are just not reused by other projects.

## How the layer key is built

The key comes from the feed content digest, not from its mount path.


- The SDK bootstrap layer (L0) does not depend on where the feed directory is.
  The target base layer (L1) also hashes the `path:` value as written in
  `avocado.yaml`, so two projects that write the same path share it and two
  that write different paths (`./a` and `./b`, or absolute paths) rebuild it,
  along with the project layer (L2) built on it.
- Re-indexing after changing the RPMs in a `path:` feed produces a new
  `repomd.xml`, so a new key. Run `avocado feed index` after every change to a
  feed directory. The key only sees `repomd.xml`, and a layer hit skips the
  package manager and its checksum checks. An RPM replaced in place (same name,
  version and release) without re-indexing therefore reuses the old layer.
- If a feed's `repomd.xml` changes while `sdk install` runs, after it computed
  its layer keys, the install commits nothing shared. It prints
  `Stacks: <layer> built in the view, not committed: the feed '<name>' changed
  ...`, and the next install computes keys from the new content.

## Reading the output

Each `sdk install` layer prints one line, on stderr so it also shows under
`avocado install`'s progress display (but not with `--output json`):
`Stacks: <layer> ... hit <key>` when the layer was reused,
`... committed <key>` when it was built, or `... already in the view <key>`
when the project already had it. A second project with identical inputs
prints:

```text
Stacks: L0 sdk bootstrap hit cc04d440fb83
Stacks: L1 target base hit 84213d54f44d
```
