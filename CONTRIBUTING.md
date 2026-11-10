# Contributing to Quarry

Thanks for helping. Quarry is a native SQL client written entirely in Rust, and the goal is a tool
that stays fast at any data size: results stay columnar, cells are formatted only while on screen,
and the UI never waits on the database. Rust and the tools come through mise.

Quarry is early: pre-1.0, with no stability promise for its crates or settings yet. That makes now
a good time for changes to how it's put together.

```sh
mise trust
mise install               # rust and prek, pinned in mise.lock
mise exec -- prek install  # run the hooks on every commit
mise run run               # start Quarry with an in-memory database
mise run dev               # the same, rebuilt and restarted whenever a source file changes
mise run test
mise run fmt               # cargo fmt
mise run lint              # clippy with -D warnings
mise run pre-commit        # what CI's pre-commit job runs
```

The first build compiles DuckDB from source, which takes several minutes; later builds are
incremental. On Linux, install GPUI's system libraries first with `script/install-linux-deps.sh`.

GPUI can't swap code into a running app, so `mise run dev` does the next best thing: it watches
`crates/` and `Cargo.toml` with watchexec (through `mise x`, so nothing to install) and restarts
Quarry after each rebuild, which takes a few seconds for a change in the app crate. Arguments after
`--` reach Quarry, as with `mise run run`.

Every dev build has a new code signature, so macOS asks once per build before Quarry reads a
password an earlier build saved in the Keychain. Signed releases keep their identity, so "Always
Allow" sticks for them.

CI runs `pre-commit` and `test` on every pull request, on GitHub-hosted Linux runners. Check UI
changes by running the app yourself, on macOS if you can, and say what you tried in the template.

## Layout

| Path | What goes there |
|---|---|
| `crates/quarry-core/` | Engine-agnostic pieces: the `Driver` trait, the threaded `Session`, the Arrow-backed `ResultSet`, the catalog model and SQL statement splitting. No UI and no engine code. |
| `crates/quarry-duckdb/` | The DuckDB driver. |
| `crates/quarry-app/` | The desktop app, built on GPUI and GPUI Kit. |
| `assets/` | The icon and the bundled JetBrains Mono font. |
| `script/` | macOS bundling and signing, and the Linux dependency installer. |

## Guidelines

- Keep the hot paths lazy. Don't turn a whole result into rows or strings; format what's visible.
- Never block the UI thread on the database. Engine calls go through a `Session`.
- A new engine is a new crate implementing `quarry_core::Driver`, with tests that drive it through
  a `Session` the way `quarry-duckdb`'s do.
- Engine errors should reach the user as the engine's own message, not wrapped in Rust detail.

## Pull requests

- Keep each pull request to one change, with a [Conventional Commits](https://www.conventionalcommits.org)
  title such as `feat(grid): copy cells as TSV` or `fix(duckdb): read views in attached databases`.
- Fill in the template, including the commands you ran.
- Never paste connection strings with passwords, tokens or private data into an issue, a pull
  request, a test or a commit.

## Releases

The version under `[workspace.package]` in `Cargo.toml` decides releases. A pull request that bumps
it releases when it merges: once CI passes on `main`, Tag release tags `v<version>` and starts
Release, which builds the macOS `.dmg` and Linux tarballs and publishes them on GitHub.

Security problems go through a private report, not an issue: see [SECURITY.md](SECURITY.md).
