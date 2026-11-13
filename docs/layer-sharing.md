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
naming the first such feed. Nothing is lost: the project builds and uses its own
layers, they are just not reused by other projects.

## How the layer key is built

The key comes from the feed content digest, not from its mount path.

- The same RPMs in two different directories give the same layers.
- Changing the RPMs in a `path:` feed produces a new `repomd.xml`, so a new key.
  A changed feed never reuses an old layer.

## Reading the output

Each `sdk install` layer prints one line: `Stacks: <layer> ... hit <key>` when
the layer was reused, or `... committed <key>` when it was built. A second
project with identical inputs prints:

```text
Stacks: L0 sdk bootstrap hit cc04d440fb83
Stacks: L1 target base hit 84213d54f44d
```
