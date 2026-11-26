# 0008: MySQL and MariaDB use mysql_async, prepared first, with the system's roots added

## Context

The MySQL driver needs streaming results, cancel from a second connection, MariaDB's ed25519
authentication, and rustls on the ring provider only. mysql_async has all of these. It builds its
own rustls configuration, though, so it cannot take the shared one the other drivers use, and it
always asks the server for multi-statement support, so the protocol would run every statement in
a text.

## Decision

Use mysql_async with `default-rustls-ring` and `client_ed25519`. Express the shared trust rules
through its options: a custom CA file is the only trusted root; otherwise the operating system's
roots are loaded once and added to the bundled ones; the certificate is checked against the
configured host name (or the override), also when the connection goes through an SSH tunnel; a
client key, decrypted with its passphrase when needed, is handed over as PEM in memory.

Every statement is prepared before it runs. MySQL cannot prepare two statements at once, so the
text is proven to be the single statement the guardrails reviewed. Commands the prepared protocol
does not support run through the text protocol only after Treasury's splitter finds exactly one
statement.

## Consequences

Trust behaves like the other engines on desktop. On Android and iOS, where the system roots cannot
be listed, the bundled roots and any custom CA apply. Each statement costs a prepare round trip.
`UPDATE` reports matched rows (`CLIENT_FOUND_ROWS`), so a grid edit that writes an unchanged value
still confirms it found its row.
