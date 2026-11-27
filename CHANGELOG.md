# Changelog

Notable changes to Treasury, grouped as Added, Changed and Fixed. Versions follow Semantic
Versioning.

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

### Fixed

- sql: Keep the script splitter on character boundaries
- sql: Treat T-SQL lock hints and CREATE OR REPLACE with care
- sql: Catch writes hidden in EXPLAIN options and WITH clauses
- driver-postgres: Stop a statement's time limit when it ends
- drivers: Report rejected certificates as TLS errors
- runtime: Say "Done" for statements that change no rows
- runtime: Keep no history for production connections unless they ask for it
- app: Say "1 row changed" on the result tab
