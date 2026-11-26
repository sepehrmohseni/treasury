# 0004: Query text is sealed at rest; history search indexes in memory

## Context

Query history, saved query text and unsaved editor buffers can contain customer data in
literals. The brief asks for field-level encryption of that text with a key in the SecretStore,
and for history search with SQLite FTS5. An FTS5 index on disk is plaintext, so it would undo the
encryption.

## Decision

The text columns hold XChaCha20-Poly1305 ciphertext, with the table and column bound in as
associated data. The key is 32 random bytes kept in the SecretStore under
`treasury/store-field-key`. The FTS5 table lives in SQLite's temp schema with
`temp_store = MEMORY`: it is built from the decrypted rows the first time the user searches in a
session, kept current as entries are added, and never written to the file. Purging history
deletes the rows and runs `VACUUM` with `secure_delete` on.

## Consequences

The first search of a session costs one decryption pass over history (milliseconds for
thousands of entries). If the keychain entry is lost, old text cannot be decrypted; those rows
are skipped by search and removed by purge, and the rest of the app keeps working.
