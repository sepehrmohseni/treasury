# Decisions

Short records of choices a future maintainer might question. Each one states the context, the
decision and what it costs. Add a new file rather than rewriting an old one; mark a replaced record
as superseded and link the one that replaces it.

| No. | Decision |
|---|---|
| [0001](0001-panic-unwind.md) | Release builds unwind on panic |
| [0002](0002-ts-rs-bindings.md) | IPC types reach TypeScript through ts-rs |
| [0003](0003-checks-through-xtask.md) | Every check is a `cargo xtask ci` step, run in a pinned image |
