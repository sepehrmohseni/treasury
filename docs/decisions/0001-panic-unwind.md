# 0001: Release builds unwind on panic

## Context

A driver decodes untrusted server responses. A bug in a driver or in one of its dependencies can
panic. The brief requires that a panic inside a driver task becomes an error for that request and
never ends the process, because the user may have unsaved work in other tabs.

`panic = "abort"` gives smaller binaries and is a common release setting, but it terminates the
whole process on the first panic.

## Decision

`[profile.release]` keeps `panic = "unwind"`. Driver work runs in tokio tasks; the core turns a
`JoinError` that `is_panic()` into a request error and closes only the affected session.

## Consequences

Binaries are somewhat larger than with abort. Code must not rely on panics as control flow, and
the workspace denies `unwrap`, `expect` and `panic!` outside tests (see the lints in Cargo.toml).
