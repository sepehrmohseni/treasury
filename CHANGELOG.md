# Changelog

Notable changes to Treasury, grouped as Added, Changed and Fixed. Versions follow Semantic
Versioning.

## 0.1.2 (2026-11-27)

### Added

- redis: Show and change a key's expiry beside its rows
- site: Download cards per platform and a list of every version
- site: Offer the download for the visitor's system, latest version only

### Fixed

- ui: Fit the window to the screen and show why a connection failed

## 0.1.1 (2026-11-27)

### Added

- redis: Edit hashes, sets, sorted sets, lists and strings in the grid

### Fixed

- release: Collect the packages in a folder the repository does not have

## 0.1.0 (2026-11-27)

### Added

- Add the Treasury mark and app icon sources
- core: Add value model, settings, driver traits and result cursor
- core: Enforce production guardrails before statements run
- sql: Classify statements, add row limits and derive count previews
- vault: Store secrets in the platform keychain or a sealed file
- store: Keep connections, history and tabs in a private SQLite file
- tunnel: Add SSH tunnels and the shared TLS client configuration
- core: Map values to JSON with exact numbers and Extended JSON
- sql: Render identifiers and literals for each dialect
- export: Stream results to CSV, NDJSON, JSON, Parquet and SQL
- import: Bring connections over from DBeaver, Beekeeper Studio and others
- cli: Add the treasury command-line tool
- driver-postgres: Add the PostgreSQL driver
- driver-mysql: Add the MySQL and MariaDB driver
- driver-mssql: Add the SQL Server driver
- cli: Ping, query and export through the guardrails
- tunnel: Add a TLS bridge for clients that make their own TLS
- driver-mongo: Add the MongoDB driver
- runtime: Connections, guardrail flow and running statements
- sql: Token spans for highlighting statements
- app: The desktop app, with its interface written in Rust
- edit: Find the table and key behind a result, and turn grid edits into a changeset
- app: Edit rows in the grid, a settings dialog, and a check for new versions
- app: Double-click a table in the tree to see its rows
- app: Several editor tabs per connection, and saved queries
- app: Search the history of a connection and open what ran
- sql: Lay statements out for reading without changing them
- app: Format the selection or the tab, as an edit Ctrl+Z undoes
- runtime: Read a statement to its end, so a script runs in order
- app: Run every statement of a tab, one after another
- sql: Find the tables a statement names, and quote names only when needed
- runtime: A catalog of each connection's tables, views and collections
- app: Suggest table, column and keyword names as a statement is typed
- app: A command palette for connections, tables, saved queries and actions
- app: SSH tunnels, certificates and the sign-in method in the connection form
- export: Markdown tables, and tab-separated values without a header
- runtime: Copy ranges of a result, add rows, and edit MongoDB documents
- app: Select and copy ranges of the grid, and add rows in it
- redis: The Redis and Valkey driver, for one server or through Sentinel
- app: Redis and Valkey connections, keys in the tree
- redis: Redis Cluster, and pages read only as the result asks for them
- app: Set up a Redis Cluster in the connection form, by its nodes

### Fixed

- sql: Keep the script splitter on character boundaries
- sql: Treat T-SQL lock hints and CREATE OR REPLACE with care
- sql: Catch writes hidden in EXPLAIN options and WITH clauses
- driver-postgres: Stop a statement's time limit when it ends
- drivers: Report rejected certificates as TLS errors
- runtime: Say "Done" for statements that change no rows
- runtime: Keep no history for production connections unless they ask for it
- app: Say "1 row changed" on the result tab
- store: Leave rows sealed under another key out of lists
- runtime: Record each statement as it starts, and complete it at the end
- app: Deleting a connection removes its history too
