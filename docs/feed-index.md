# avocado feed index

`avocado feed index <dir>` writes `<dir>/repodata/` for a directory of RPMs by
running the host `createrepo_c`. The metadata is regenerated on every run;
there is no `--update`. The directory is passed to `createrepo_c` as a single
argument, with no shell involved.

```sh
avocado feed index /abs/dir
```

The metadata revision and timestamps are set to the modification time of the
newest `.rpm` under the directory, so indexing an unchanged directory again
writes an identical `repomd.xml`. Adding, removing or changing an RPM changes
`repomd.xml` through the package checksums, even when the revision stays put
(an RPM copied with `cp -p` keeps its older modification time).
`createrepo_c`'s own warnings are shown, so an unreadable RPM is named.

## Errors

- `createrepo_c` not on PATH: the error names `createrepo_c` and says how to
  install it.
- The directory does not exist or is not a directory: the error names the path.

## Using the result as a feed

Declare the directory under top-level `repos:` with `path:`, and enable it in
`distro.feeds`. A `repos:` entry is only a definition; without `distro.feeds`,
dnf does not read it.

```yaml
distro:
  feeds: [my-feed, avocado]

repos:
  my-feed:
    path: /abs/dir
```

List the feed before `avocado` so it wins on priority. dnf still picks the
highest version across enabled feeds unless a package is pinned:

```yaml
packages:
  htop: "3.5.0-r0.1"
```
