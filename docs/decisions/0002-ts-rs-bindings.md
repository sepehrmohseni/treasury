# 0002: IPC types reach TypeScript through ts-rs

Superseded by [0011](0011-ui-in-rust-with-leptos.md): the interface is written in Rust.

## Context

The UI and the Rust core must not drift apart: every type that crosses the Tauri IPC boundary
needs a TypeScript definition generated from Rust. The two candidates are tauri-specta, which also
types the commands, and ts-rs, which generates type definitions only.

As of November 2026 the tauri-specta line for Tauri 2 is still a release candidate (the latest
stable release, 1.0.2, targets Tauri 1). ts-rs 12 is stable.

## Decision

Use ts-rs. Types derive `TS` and are exported by `cargo test` into `ui/src/ipc/bindings/`; a check
step fails when the committed files differ from a fresh export. Command names and signatures are
declared once in a small typed wrapper in the UI that only uses the generated types.

## Consequences

Command signatures are written by hand on the TypeScript side, so a renamed command is caught by
the end-to-end tests rather than by the compiler. Revisit when a stable tauri-specta for Tauri 2
ships.
