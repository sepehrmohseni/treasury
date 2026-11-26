# 0010: MongoDB speaks database commands; results keep every field

## Context

MongoDB has no query text in the SQL sense. Tools either embed a JavaScript shell or invent a
syntax. The guardrails need to know what a statement does before it runs, which a JavaScript
shell makes impossible to decide reliably. Documents in one result also have different fields,
while a grid needs its columns before the first row is shown.

The official driver's rustls build cannot verify a certificate against a name other than the
address it dials, which a connection through an SSH tunnel needs.

## Decision

The editor language is MongoDB's own database commands written as Extended JSON
(`{"find": "orders", "filter": {...}}`), the form every driver sends. A command table classifies
them; aggregations with `$out` or `$merge` write; updates and deletes with an empty filter are
flagged; the preview counts the documents an update or delete matches with the same filter. One
command runs at a time; `"$db"` picks another database.

A result's columns are the top-level fields of its first batch, `_id` first, plus a last column
that holds any field a later document adds, so nothing is dropped. Rows are read from raw BSON
without building a parsed document each.

For one server, directly or through a tunnel, Treasury makes the TLS itself through a loopback
bridge with the shared TLS configuration, and the driver connects to the bridge. For replica set
discovery, which dials every member by name, the driver's own TLS is used.

## Consequences

Users who know the shell syntax write commands instead; the command reference applies as is.
Read-only is enforced by the guardrails, as MongoDB has no read-only session. Transactions for
grid edits exist only on replica sets; on a standalone server edits run in order and an error
says how many were saved.
