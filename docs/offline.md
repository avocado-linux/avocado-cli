# Offline mode

`--offline` (or `AVOCADO_OFFLINE` set to `1`, `true` or `yes`) makes the avocado process itself open no
network connection. It is CLI-side only. It does not make the package manager
inside the SDK container work offline.

## What it does

With `--offline` or `AVOCADO_OFFLINE=1` set, avocado:

- skips the latest-snapshot pointer lookup,
- skips the snapshot pin HEAD check,
- skips the feed preflight (every command that goes through
  `preflight_feeds_once` and `resolve_and_apply`),
- uses the locked pin from the lock file.

It prints the pin it used once. The locked pin is whatever the last online
`avocado update` recorded, so an old locked pin installs old packages. Check
the printed pin and run `avocado update` online when you need newer packages.

## No locked pin

When the lock file has no pin and the distro feed is the default avocado
repository, `--offline` stops before any request with an error that names the
missing pin and tells you to run `avocado update` while online. Run it once
online to record a pin.

Any other `distro.repo.url` (a mirror, an `http://` spelling of the default,
an explicit port) proceeds unpinned with a notice instead of failing, because
avocado cannot tell whether that repository publishes snapshots. If it does,
the run resolves the moving channel, not a snapshot.

## Commands that need the network

- `avocado update` looks up the channel's latest snapshot, so it fails under
  `--offline` with an error saying so.
- A private `org:` feed needs a short-lived Connect token minted over HTTPS.
  When an enabled `org:` feed has no token yet, `--offline` fails naming the
  feed instead of opening a connection.

## What it does not do

The package manager (dnf) inside the SDK container still fetches metadata and
packages from remote feeds. The distro feed is a remote feed, so an install that
needs a remote feed fails with the package manager's own error unless the
network is available. Fully network-less installs need a persisted package
cache, which avocado does not provide yet.

## Overrides

Setting `AVOCADO_SDK_REPO_RELEASE`, the config `releasever`, or
`AVOCADO_RELEASEVER` keeps its existing behavior under `--offline`.
