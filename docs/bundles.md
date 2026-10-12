# Provisionable bundles and their dependencies

`avocado provision --pack` (and `--rubicon`, which packs) captures what the
SDK's provision hook needs as a content-addressed bundle; `avocado provision
--bundle` replays it on another machine, usually a site hub. The hub has no
build tree, none of the build site's paths, and none of its credentials. A
bundle is only useful if everything it needs is either inside it or
reachable from the hub.

## What a bundle carries

| Input | How the hub gets it |
| --- | --- |
| Runtime images, BSP bootfiles, stone manifest and scripts | In the bundle (blobs) |
| The project config and lock | In the bundle (`bundle.json`) |
| Rubicon run profiles | In the bundle, resolved at pack time |
| Path-sourced extensions (`source: { type: path }`) | In the bundle: one tar.gz each; the bundled config points at the copy |
| Package-sourced extensions | Fetched by the hub from the package feed, pinned by the lock |
| The SDK | Installed by the hub from the package feed, pinned by the lock |
| Git-sourced extensions | Fetched by the hub from the repository |

## Path sources

At pack time each path-sourced extension's directory is archived
(deterministic: sorted, owner 0, mtime 0; `.git` and `.avocado*` left out)
and stored as a blob. The bundled `avocado.yaml` points the source at
`.avocado-bundle/ext/<name>`, where the hub unpacks it before `sdk install`,
so the install, the config merge and the extension's own SDK contributions
all resolve locally.

Before this, a hub fetched the path as written, relative to its own work
directory. It existed only by accident (a work directory left over from an
earlier run), and a fresh target failed in `sdk install`.

## Private git, private feeds: the rule

**Anything the hub cannot fetch anonymously is carried, or the hub is given
a credential scoped to the job.**

- **Private git extensions:** carry them like path sources. The build site
  already has the checkout (the fetch placed it under the SDK's
  `includes/`), so pack archives that tree instead of making the hub clone.
  Same mechanism, a different directory. Until then, a git source works on
  a hub only if the repository is public.
- **Private package feeds** (org feeds, entitled BSPs): the SDK and the
  package-sourced extensions are installed from the feed by the hub. Two
  options, both compatible with the lock:
  1. **Job-scoped feed credential (preferred):** Connect already mints a
     short-lived token per command for the Rubicon; it mints a feed token
     the same way, scoped to the feeds the bundle's lock names and expiring
     with the job, and passes it with the provision command. The hub never
     stores a long-lived feed secret.
  2. **Carry the packages:** pack the RPMs the lock pins into the bundle and
     point the hub's install at them. Larger bundles, but works for
     air-gapped sites too (together with a carried SDK).
- **Offline sites:** carry the SDK as well (a prebuilt SDK per host arch,
  content-addressed like the images). Same bundle format, more blobs.

## Status

| Case | Today |
| --- | --- |
| Path source | Carried (bundle field `ext_sources`) |
| Public git source | Fetched by the hub |
| Private git source | Not yet: carry the build site's checkout |
| Public package feed | Fetched by the hub |
| Private package feed | Not yet: job-scoped feed token from Connect |
| No network at the site | Not yet: carry packages and the SDK |
