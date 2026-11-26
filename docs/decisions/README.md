# Decisions

Short records of choices a future maintainer might question. Each one states the context, the
decision and what it costs. Add a new file rather than rewriting an old one; mark a replaced record
as superseded and link the one that replaces it.

| No. | Decision |
|---|---|
| [0001](0001-panic-unwind.md) | Release builds unwind on panic |
| [0002](0002-ts-rs-bindings.md) | IPC types reach TypeScript through ts-rs |
| [0003](0003-checks-through-xtask.md) | Every check is a `cargo xtask ci` step, run in a pinned image |
| [0004](0004-history-encryption-and-search.md) | Query text is sealed at rest; history search indexes in memory |
| [0005](0005-limit-rewrite-verified-by-reparse.md) | The row limit is inserted into the user's text and verified by parsing again |
| [0006](0006-secret-stores.md) | Secret stores through keyring-core, with a sealed file as the fallback |
| [0007](0007-postgres-results-through-the-simple-protocol.md) | PostgreSQL results stream through the simple protocol, after a prepare |
