# 0003: Every check is a `cargo xtask ci` step, run in a pinned image

## Context

Checks that only exist in a workflow file drift from what developers run locally, and the reverse.
Contributors may not have Rust, Node, pnpm or the WebKitGTK headers installed.

## Decision

Each check is a step in `xtask/src/ci.rs`. GitHub Actions jobs call `cargo xtask ci <step>`;
locally, `scripts/docker-check.sh` builds `docker/ci/Dockerfile` (pinned Rust, Node, pnpm and cargo
tools, each verified against its published checksum) and runs `cargo xtask ci` in it against a
read-only copy of the checkout. A step that cannot run (a missing tool, no Docker socket for the
integration suites) reports itself as skipped and is listed in the summary; it never counts as
passed.

## Consequences

Changing a check means editing Rust, not YAML. The image is large (WebKitGTK, Xvfb), and the first
build takes several minutes; later runs reuse the layers and the cache volumes.
