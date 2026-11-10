<p align="center"><img src="assets/icon/png/quarry-128.png" width="96" alt="Quarry icon"></p>

# Quarry

A fast, native, open-source SQL client, written entirely in Rust.

Quarry aims to be a lightweight alternative to IDE-style database tools: a
GPU-rendered UI that starts instantly and keeps scrolling smooth through
millions of rows. It speaks DuckDB, SQLite, PostgreSQL, MySQL, MariaDB and
ClickHouse, and adapts to the release of the engine it connects to.

## Status

Early. What works today:

- Open a DuckDB or SQLite file (⌘O, the engine is picked from the file's header), start an
  in-memory database, or connect to PostgreSQL, MySQL, MariaDB or ClickHouse by URL (⇧⌘O)
- Version-aware: each driver reads the server's release and adapts its catalog queries,
  and the editor colors the keywords, functions and types of that dialect *at that release*.
  A function or statement the server does not have yet (or no longer has) gets a warning,
  e.g. `regexp_count() needs PostgreSQL 15 or later; this server is 14.13`
- Dialect-accurate highlighting in JetBrains Mono: MySQL `#` comments and backslash escapes,
  PostgreSQL dollar quotes and `E''` strings, SQLite `[brackets]`, ClickHouse `{param:Type}`
- Scripts split per dialect, including the mysql client's `DELIMITER` for routine bodies
- Run the statement under the caret (⌘↵), the selection, or the whole script (⇧⌘↵)
- Editor tabs, each with its own text, caret, undo history and SQL flavor: switch with a click,
  ⌘1…⌘9, ⇧⌘] / ⇧⌘[ or ⌃Tab, close with ⌘W (or a middle click), reopen a closed one with ⇧⌘T
- Query consoles per connection (⌘T): the tabs show the consoles of the connection you are on,
  and each connection gets one when you first open it. Consoles and scratches (⇧⌘N) save as you
  type, under Quarry's settings folder, and come back after a restart
- SQL files: open (⌥⌘O, or `quarry path/to/file.sql`), create (⌘N), Save (⌘S) and Save As (⇧⌘S).
  A dot marks unsaved changes, closing asks first, and quitting keeps them for next time
- Cancel a running query (⌘.)
- Saved connections: every connection made with + (⇧⌘O) is kept in the sidebar, with its password
  in the macOS Keychain or the Secret Service, never in the settings file. Test Connection checks the
  fields before saving; right-click a connection to edit or delete it. `quarry <name>` opens one
- Results stream into a virtualized grid as the engine produces them, with the engine's own
  type names in column headers (`jsonb`, `decimal(10,2)`, `LowCardinality(String)`)
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
| `quarry-core` | Engine-agnostic pieces: the `Driver` trait, threaded `Session`, Arrow-backed `ResultSet`, catalog model, and per-dialect SQL: tokenizer, statement splitting, versioned vocabularies and highlighting |
| `quarry-duckdb` | The DuckDB driver (bundled DuckDB with the JSON and Parquet extensions) |
| `quarry-sqlite` | The SQLite driver (bundled SQLite; column types chosen per batch from the values) |
| `quarry-postgres` | The PostgreSQL driver |
| `quarry-mysql` | The MySQL and MariaDB driver |
| `quarry-clickhouse` | The ClickHouse driver (HTTP, results streamed as Arrow) |
| `quarry-app` | The desktop app (`quarry` binary) |

A new engine implements `quarry_core::Driver`: report its `Flavor` (dialect and
release), run a statement into a `BatchSink`, read the catalog, and expose a
thread-safe canceller. Its dialect gets a `Lexical` in `quarry_core::dialect` and
word lists in `quarry_core::vocabulary`, where `word@15` marks the release that
added a word and `word!8.0.11` the one that removed it.

The server drivers' integration tests run when `QUARRY_TEST_POSTGRES_URLS`,
`QUARRY_TEST_MYSQL_URLS` or `QUARRY_TEST_CLICKHOUSE_URLS` hold comma-separated
URLs (CI runs two releases of each engine), and skip otherwise.
`QUARRY_TEST_UPDATE_INSTALL` names a throwaway `Quarry.app` (or Linux `quarry` binary) for a test
that installs the latest release over it, the way the updater does.
`QUARRY_CONFIG_DIR` points Quarry at another settings folder (saved connections, consoles,
scratches and open tabs), e.g. for trying a build without touching your own.

## Install

Download the latest build from [Releases](https://github.com/ggoggam/quarry/releases): a universal
`.dmg` for macOS and tarballs for Linux on x86_64 and aarch64.

Quarry keeps itself up to date: shortly after launch and once a day, it checks GitHub for a newer
release and asks before installing it (Quarry > Check for Updates… checks now). An update is
checked against the release's `SHA256SUMS` and, on macOS, must be signed by the same developer as
the installed app; it takes effect when Quarry restarts. Where Quarry can't write to its own folder,
the prompt links to the release page instead. Set `QUARRY_UPDATE_CHECK=0` to turn the automatic
checks off.

## Building

Tools come through [mise](https://mise.jdx.dev): Rust and [prek](https://github.com/j178/prek) are
pinned in `mise.lock`. The first build compiles DuckDB from source and takes a while; later builds
are incremental.

```sh
mise install
mise run run                            # in-memory database
mise run run -- path/to/database.duckdb
mise run run -- path/to/app.sqlite
mise run run -- postgres://user@localhost:5432/app
mise run run -- shop.duckdb report.sql      # a database, and a SQL file in a tab
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
- More engines

## License

Apache-2.0. The bundled JetBrains Mono font is under the SIL Open Font License
(`assets/fonts/jetbrains-mono/OFL.txt`).
