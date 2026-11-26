# 0007: PostgreSQL results stream through the simple protocol, after a prepare

## Context

PostgreSQL has two ways to run a statement. The extended protocol binds parameters and returns
values in binary, which tokio-postgres decodes only for types it knows; extension types (PostGIS,
hstore, citext, ltree) and user types come back as bytes. The simple protocol returns every value
in the server's own text form, which is what users expect to see, but runs every statement in the
text it is given, so a stray `; DROP TABLE` would run along with the statement the guardrails
checked.

Results must also stream: a query returning millions of rows cannot be buffered.

## Decision

Every statement is first prepared. Preparing reports the column types and fails when the text
holds more than one command, which the driver refuses with a clear error. Statements without
parameters then run through the simple protocol and are decoded from text, with the prepared
types deciding how each value is read. Statements with parameters run through the extended
protocol, with a binary decoder for the common types and the server's text input syntax for any
parameter type without a binary encoding.

Streaming comes from tokio-postgres itself: responses reach the reader through a channel that
holds one message, so when Treasury stops reading, the connection stops reading the socket and the
server waits. No portal row count or cursor is needed for memory to stay bounded.

## Consequences

Every type displays as PostgreSQL writes it, including types Treasury has never heard of. Each
statement costs one extra round trip for the prepare. A test compares the binary and text decoders
on the same row so the two paths cannot drift apart. `COPY` is refused; Export and Import cover
files.
