# Treasury

Treasury is an open-source database client for PostgreSQL, MySQL, MariaDB, SQL Server, MongoDB,
Redis and Valkey, with a desktop app and a command line tool. It reads each statement before the
server does: it knows which ones write, shows how many rows an UPDATE or DELETE would change, and
keeps production connections read-only until you unlock them.

It has no account, no paid tier and no telemetry. Downloads for Linux, macOS and Windows are on
the [project page](https://sepehrmohseni.github.io/treasury/) and with each
[release](https://github.com/sepehrmohseni/treasury/releases). What comes next is in the
[roadmap](docs/roadmap.md).

## Desktop app

- Add connections by hand, from a connection string, or by importing the export files of other
  clients. Passwords go to the system keychain, never to a settings file. SSH tunnels (with a
  jump host), client certificates and a custom CA are set in the connection form.
- Browse databases, schemas, tables and collections, or Redis keys grouped by `:`, in a tree that
  loads as you open it.
- Work in several tabs per connection, kept with their text across restarts, and save tabs as
  queries in folders. Search the history of what ran on each connection.
- Write statements in an editor that highlights them and suggests names; run the one under the
  cursor with Ctrl+Enter, or all of them in turn with Shift+Ctrl+Enter. Plain SELECTs get a
  1,000-row limit unless you turn it off. Ctrl+K finds connections, tables, saved queries and
  actions.
- Read results in a grid that only fetches the rows in view, so large results stay fast; copy a
  selection as TSV, CSV, JSON, Markdown or INSERT statements.
- Edit, add and delete the rows of a result that comes from one table, edit the documents a
  MongoDB find returned, and edit what a Redis key holds (hash fields, set and sorted set
  members, list entries, a string's value). Changes collect in the grid, and nothing is saved
  until you have read the statements they become; they are saved together.
- Export a result to CSV, JSON, NDJSON or Parquet. Export streams to the file.

The app is a [Tauri](https://tauri.app) window around the Rust core; its interface is written in
Rust too ([ADR 0011](docs/decisions/0011-ui-in-rust-with-leptos.md)).

## Building and checking

Every check CI runs is a step of `cargo xtask ci`. Without a local Rust toolchain, run them in
Docker:

```sh
scripts/docker-check.sh            # every check, as CI runs them
scripts/docker-check.sh ci --list  # the steps
scripts/docker-check.sh ci clippy  # one step
scripts/docker-check.sh fmt        # format the checkout
```

The script mounts the checkout read-only, keeps build outputs and caches in Docker volumes, and
uses half of the CPU cores unless `TREASURY_CI_CPUS` says otherwise.

The `desktop` step builds the app and drives its window through `tauri-driver` against
PostgreSQL in a container, the way a person would use it. To work on the interface itself, run
`cargo tauri dev` in `crates/app` with Rust, [trunk](https://trunkrs.dev) and the
`wasm32-unknown-unknown` target installed.

## Command line

`treasury` runs statements through the same guardrails as the app. It supports PostgreSQL, MySQL,
MariaDB and SQL Server so far. Keep the password out of the connection string; it is read from
`TREASURY_PASSWORD` (or the variable named by `--password-env`).

```sh
treasury ping  postgres://app@db.internal:5432/shop
treasury query postgres://app@db.internal:5432/shop "SELECT * FROM orders WHERE status = 'open'"
treasury query --write --production mysql://app@db.internal/shop "DELETE FROM carts WHERE expired"
treasury export --format parquet --out orders.parquet postgres://app@db.internal/shop "SELECT * FROM orders"
```

Sessions are read-only unless `--write` is given. On shared databases a write first shows how
many rows it changes; with `--production`, destructive statements, statements without `WHERE`
and changes above 1,000 rows ask for the connection name, which can only be typed at a terminal.

## Project page

The page at [sepehrmohseni.github.io/treasury](https://sepehrmohseni.github.io/treasury/) is
plain HTML and CSS in `site/`. Its statement table comes from the analyzer and guardrails, and a
test fails when the page and the code disagree. After changing either, print the new rows with
`scripts/docker-check.sh -- cargo run -q -p treasury-tests --example site-statements` and put
them between the `statements` markers in `site/index.html`. The downloads are filled in from the latest
release when the page is built (`scripts/site-downloads.py`), and `site/download.js` picks the
system the visitor's browser reports; `scripts/test_site_downloads.py` and
`scripts/test_download_os.js` test both.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
