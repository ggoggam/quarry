<p align="center"><img src="assets/icon/png/quarry-128.png" width="96" alt="Quarry icon"></p>

# Quarry

A fast, native, open-source SQL client, written entirely in Rust.

Quarry aims to be a lightweight alternative to IDE-style database tools: a
GPU-rendered UI that starts instantly and keeps scrolling smooth through
millions of rows. It starts with [DuckDB](https://duckdb.org) and is built so
more engines can plug in behind the same interface.

## Status

Early. What works today:

- Open a DuckDB file or start an in-memory database
- SQL editor with tree-sitter highlighting, set in JetBrains Mono
- Run the statement under the caret (⌘↵), the selection, or the whole script (⇧⌘↵)
- Cancel a running query (⌘.)
- Results stream into a virtualized grid as DuckDB produces them, with typed column headers
- Schema browser: databases, schemas, tables, views and columns; double-click a table to preview it
- Light and dark themes

## Why it's fast

- **Results stay columnar.** Rows arrive as Arrow record batches straight from
  DuckDB's streaming API and are never copied into rows of strings. A cell is
  turned into text only while it's on screen.
- **Streaming fetch.** The first rows show while the rest are still being
  produced, and fetching stops at the row limit (1,000,000 by default) without
  materializing the remainder.
- **The UI never waits on the database.** Each connection runs on its own
  thread. The UI submits work, receives batches over a channel, and can
  interrupt the engine at any time.
- **GPU rendering.** The UI is built on [GPUI](https://www.gpui.rs) (the
  framework behind Zed) and [GPUI Kit](https://gpui-kit.com), whose table
  draws only the visible rows and columns.

## Layout

| Crate | What it does |
|---|---|
| `quarry-core` | Engine-agnostic pieces: the `Driver` trait, threaded `Session`, Arrow-backed `ResultSet`, catalog model, SQL statement splitting |
| `quarry-duckdb` | The DuckDB driver (bundled DuckDB with the JSON and Parquet extensions) |
| `quarry-app` | The desktop app (`quarry` binary) |

A new engine implements `quarry_core::Driver`: run a statement into a
`BatchSink`, read the catalog, and expose a thread-safe canceller.

## Install

Download the latest build from [Releases](https://github.com/ggoggam/quarry/releases): a universal
`.dmg` for macOS and tarballs for Linux on x86_64 and aarch64.

## Building

Tools come through [mise](https://mise.jdx.dev): Rust and [prek](https://github.com/j178/prek) are
pinned in `mise.lock`. The first build compiles DuckDB from source and takes a while; later builds
are incremental.

```sh
mise install
mise run run                            # in-memory database
mise run run -- path/to/database.duckdb
mise run test
mise run bundle:macos                   # target/release/Quarry.app
```

On Linux, GPUI needs X11/Wayland, Vulkan and font libraries: `script/install-linux-deps.sh`
installs them on Debian and Ubuntu. See [CONTRIBUTING.md](CONTRIBUTING.md) for the rest.

## Releases

Bumping `version` under `[workspace.package]` in `Cargo.toml` releases it: when the bump lands on
`main` and CI passes, `v<version>` is tagged and the Release workflow publishes the builds.

## Roadmap

- Result tabs per statement, query history
- Copy and export results (CSV, Parquet, JSON)
- Autocomplete from the catalog
- Editing table data in the grid
- More engines: PostgreSQL, SQLite, MySQL
- Saved connections

## License

Apache-2.0. The bundled JetBrains Mono font is under the SIL Open Font License
(`assets/fonts/jetbrains-mono/OFL.txt`).
