# Roadmap

The first release, 0.1.0, is a database client for everyday work: connect, browse, query, read
results, edit rows and export them, with the guardrails around every statement. The
[README](../README.md) lists what it does. Everything below comes after it, roughly in this order.
Nothing here is promised for a date; each item ships when it is finished and tested, and the
list changes as people ask for things.

Ideas and requests are welcome as [issues](https://github.com/sepehrmohseni/treasury/issues).

## In the workspace

- Several editor tabs per connection, kept with their text across restarts.
- Saved queries in folders.
- The query history in a panel, searchable. History is already recorded and encrypted; this is
  the view of it.
- Density and font size in Settings. Theme, the default row limit, history, tips, the
  walkthrough and the version check are already there.
- A command palette (Ctrl+K) for connections, tables and settings.
- Completion of table and column names, formatting, run all statements.
- An inspector for the selected row or object, with its DDL.
- Inserting rows from the grid, and editing MongoDB documents. Updating and deleting rows of SQL
  tables is in 0.1.0.
- SSH tunnel and client certificate settings in the connection form. Imported connections keep
  theirs already.
- Copying cells and rows as TSV, CSV, JSON, Markdown or INSERT statements.

## More engines

- Redis and Valkey: standalone, Sentinel and Cluster, with a key browser.
- Cassandra and ScyllaDB.
- Snowflake and Databricks SQL. Nothing that needs a running warehouse runs without asking.
- Oracle, through Oracle Instant Client, and IBM Db2, through the IBM CLI driver.

Statement analysis for every SQL dialect above is already written and tested; the drivers come
one engine at a time.

## Mobile

Apps for Android and iOS built on the same core, with a layout made for touch. Every connection
opens read-only, writes unlock per session, and biometric unlock can be required before
connecting to production.

## Sync between your devices

Connections, saved queries and settings, synced as one end-to-end encrypted file to storage you
control: a folder (which works with any folder-syncing service), a WebDAV share, an
S3-compatible bucket, or a Git repository. Sync is off until you choose a destination; there is
no Treasury server.

## Understanding a database

- Execution plans as a tree, from EXPLAIN on each engine.
- An overview per connection: table sizes, index use, active and blocking sessions, slow
  queries. A metric your account cannot read says which grant it needs.
- Schema review: tables without a primary key, foreign keys without an index, duplicate
  indexes, each with the fix as SQL you can open in the editor.

## Optional, later

- Model integration with your own key (OpenAI-compatible endpoints, Anthropic, Gemini, Ollama):
  write a query from a description, explain a plan. It shows exactly what will be sent before
  every request, sends schema metadata only unless you choose otherwise, and its output is only
  ever editor text that goes through the same guardrails as typed SQL.
- Watchers: a saved read query or metric on a schedule, with a desktop notification or a webhook
  when its condition changes.
