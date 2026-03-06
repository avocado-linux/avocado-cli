# Automatic Update Notifications

The avocado CLI automatically checks for new releases and notifies you when a newer version is available, without requiring you to explicitly run `avocado upgrade`.

## How It Works

When you run any `avocado` command, a background task is spawned concurrently with your command to query the latest release from GitHub. After your command finishes, the result is checked and a notice is printed to stderr if a newer version exists:

```
[UPDATE] avocado 0.28.0 is available (you have 0.27.0).
         Run 'avocado upgrade' to update.
```

The check runs in the background so it does not slow down your command. A 5-second timeout limits any additional wait if the command finishes before the check completes.

## Caching

Results are cached for **24 hours** in the platform cache directory (e.g. `~/.cache/avocado/update_check.json` on Linux). No network call is made if the cache is fresh.

To force a fresh check, delete the cache file:

```sh
rm ~/.cache/avocado/update_check.json
```

## Opting Out

Set the `AVOCADO_NO_UPDATE_CHECK` environment variable to skip the check entirely:

```sh
AVOCADO_NO_UPDATE_CHECK=1 avocado build
```

To disable permanently, add it to your shell profile (`~/.bashrc`, `~/.zshrc`, etc.).

## Behavior Details

| Scenario | Result |
|---|---|
| Cache hit (checked within 24h) | No network call; <1ms overhead |
| Cache miss, network available | Fetches GitHub API concurrently; notice shown at end if newer version found |
| Cache miss, offline | Silent — no error, no notice |
| Running `avocado upgrade` | Update check skipped (you are already upgrading) |
| `AVOCADO_NO_UPDATE_CHECK` set | Update check skipped entirely |

## Implementation Notes

- **Source**: [src/utils/update_check.rs](../../src/utils/update_check.rs)
- The check calls `https://api.github.com/repos/avocado-linux/avocado-cli/releases/latest`
- Output goes to **stderr** so it does not interfere with piped stdout output
- Requires no new dependencies — uses existing `reqwest`, `serde_json`, `directories`, and `semver` crates
