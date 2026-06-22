# Updating

Synaptic can update itself in place from the latest [GitHub Release](../../releases). The
update system is **opt-in**: Synaptic never contacts the network or replaces the binary on
its own. You either run `synaptic self-update` explicitly, or enable a once-a-day
"update available" notice that only ever prints a one-line reminder.

This page covers the whole system. For the bare command reference see
[`self-update`](Commands#self-update) in [Commands](Commands); for the files and environment
variables involved see [Configuration](Configuration).

## At a glance

```sh
synaptic self-update            # check; if a newer release exists, prompt then replace
synaptic self-update --check    # report whether an update is available, then exit
synaptic self-update --yes      # update without the confirmation prompt
synaptic self-update --enable   # turn on the daily "update available" notice
synaptic self-update --disable  # turn the notice back off
```

Nothing here runs unless you ask for it. `--enable`/`--disable` only write a small config
file and need no network.

## How an update works

Running `synaptic self-update` performs these steps:

1. **Check.** Query `releases/latest` for `ColinVaughn/Synaptic` and compare the tag to the
   running version (a leading `v` is tolerated; the comparison is semantic-version aware). If
   the latest release is not newer, it prints `Synaptic is up to date (<version>).` and exits.
2. **Confirm.** If a newer release exists, it prints the version delta and the release notes,
   then asks `Download and replace the current binary? [y/N]`. Answer `y`/`yes` to proceed.
   `--yes` (or `-y`) skips this prompt for scripts.
3. **Download.** Fetch the prebuilt archive that matches your platform (see
   [Platform support](#platform-support)).
4. **Verify.** If the release publishes a `.sha256` checksum next to the archive, the
   download is verified against it; a mismatch **aborts** the update before anything is
   replaced. Releases made before checksums were published have no sidecar, so verification
   is skipped with a printed warning rather than failing.
5. **Replace.** Extract the binary and atomically replace the currently running executable.
   The `syn` short alias next to it is updated too.
6. **Refresh skills.** Re-render every agent skill recorded in
   `~/.synaptic/skills.toml` to the new version. Because the running process is still the
   old binary, this step spawns the freshly-installed one to do the rendering. Skills you
   have hand-edited are left untouched and reported; see
   [Versioning and auto-refresh](Assistant-Integration#versioning-and-auto-refresh). Run it
   on its own anytime with `synaptic install --refresh`.

The new version takes effect the next time you run Synaptic — the already-running process
keeps the old code in memory, so the command finishes with
`Updated to <version>. Restart synaptic to use the new version.`

If anything fails (network, checksum mismatch, write error), the existing binary is left
untouched: the download is verified before the swap, never overwritten in place first.

## The opt-in background notice

By default Synaptic never checks for updates. Turn the reminder on with:

```sh
synaptic self-update --enable
```

Once enabled, ordinary commands occasionally check for a newer release in the background and,
if one exists, print a single line to **stderr** and then continue normally:

```
(note) Synaptic 0.3.3 is available - run `synaptic self-update`
```

Properties of the background check:

- **Throttled.** It runs at most once every 24 hours. The timestamp of the last check is
  stored in the config file, so a burst of commands triggers only one check.
- **Non-blocking and silent on failure.** It uses a short timeout, and any network or parse
  error is swallowed — a flaky connection never slows down or breaks a normal command. The
  timestamp still advances so a failed check is not retried on every invocation.
- **stderr only.** The notice goes to stderr, so it never corrupts machine-readable stdout
  (for example a `--json` result or the `serve` MCP stream).
- **Never on `self-update` itself.** The check is skipped while you run the update command.

Turn it back off at any time:

```sh
synaptic self-update --disable
```

### Disabling the check without changing config

Set `SYNAPTIC_UPDATE_CHECK=0` to force the background check off even when the config has it
enabled. This is useful in CI or any non-interactive environment:

```sh
SYNAPTIC_UPDATE_CHECK=0 synaptic query "..."
```

The variable only affects the background notice; it does not change what `synaptic
self-update` does when you run it explicitly.

## Checksum verification

When a release publishes a `synaptic-<target>.tar.gz.sha256` (or `.zip.sha256`) sidecar next
to its archive, `self-update` downloads it and verifies the SHA-256 of the downloaded archive
before replacing the binary. A mismatch aborts the update. Synaptic's release workflow
publishes these sidecars; releases predating that workflow have none, in which case
verification is skipped with a warning so updating still works against older releases.

## Platform support

`self-update` downloads the prebuilt archive matching your platform:

| Platform | Release asset |
|---|---|
| Linux x86_64 | `synaptic-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `synaptic-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `synaptic-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `synaptic-x86_64-pc-windows-msvc.zip` |

On any other platform there is no prebuilt binary to install. `self-update` detects this,
prints the [Releases](../../releases) URL so you can download or build manually, and exits
without changing anything.

## Updating a source build

`self-update` replaces whatever binary is currently running, including one produced by
`cargo install --path bin/synaptic` or `cargo build`. Note that the replacement is the
**default-feature** prebuilt binary. If you built with extra Cargo features (for example
`pg`, `push`, `office`, `gws`, `media`, or `live-explain`; see [Installation](Installation)
and [Configuration](Configuration)), self-updating swaps in a binary that does not have them.
Rebuild from source with your features instead of self-updating in that case.

## Rate limits and tokens

The check uses GitHub's anonymous REST API, which is rate-limited per IP. If you hit the
limit (for example on a shared CI runner), set `GITHUB_TOKEN` to a token and Synaptic will
send it to raise the limit. The token is optional and only used to authenticate the release
lookup.

## Files and variables

| Path / variable | Role |
|---|---|
| `~/.synaptic/update.toml` | Stores `enabled` (the opt-in flag) and `last_check` (the throttle timestamp). Written by `--enable`/`--disable`. On Windows this is `%USERPROFILE%\.synaptic\update.toml`; with no home directory it falls back to `.synaptic/update.toml` in the working directory. |
| `~/.synaptic/skills.toml` | Registry of installed agent skills, read by the post-update refresh step (and `synaptic install --refresh`). Written by `synaptic install`/`uninstall`. See [Assistant Integration](Assistant-Integration#versioning-and-auto-refresh). |
| `SYNAPTIC_UPDATE_CHECK` | Set to `0` to force the background notice off regardless of config. |
| `GITHUB_TOKEN` | Optional. Raises the GitHub API rate limit for the release lookup. |

See [Configuration](Configuration) for the full list of files and environment variables, and
[Commands](Commands#self-update) for the command reference.
