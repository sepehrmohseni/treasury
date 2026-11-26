#!/bin/sh
# Entry point of the treasury-ci image: copies the read-only checkout from /src into the container
# and runs `cargo xtask` there. See scripts/docker-check.sh.
set -eu

usage() {
  cat <<'EOF'
usage: treasury-ci [xtask arguments...|shell|bundle|-- command...]
  ci [STEP...]   cargo xtask ci (the default): every check CI runs, or only the named steps
  shell          an interactive shell in the prepared copy
  bundle         build the desktop app's Linux packages into /out
  -- cmd         run any command in the prepared copy
Anything else is passed to `cargo xtask`.
EOF
}

# Some tools (OpenSSH among them) refuse to run for a user id without an account.
if ! getent passwd "$(id -u)" >/dev/null 2>&1; then
  echo "ci:x:$(id -u):$(id -g):treasury ci:${HOME}:/bin/sh" >>/etc/passwd
fi
USER="$(id -un)"
export USER
mkdir -p "$HOME"

# Cargo never deletes old build outputs, so the target cache only grows: every change to the code,
# the dependencies or the toolchain leaves the previous outputs behind, and the app adds release,
# debug and WebAssembly builds of its own. Start it afresh once it passes TREASURY_CI_TARGET_GB
# (30 GB by default), so it cannot fill the disk.
limit_gb="${TREASURY_CI_TARGET_GB:-30}"
case "$limit_gb" in '' | *[!0-9]*) limit_gb=30 ;; esac
used_kb="$(du -sk /cache/target 2>/dev/null | cut -f1)"
if [ "${used_kb:-0}" -gt $((limit_gb * 1024 * 1024)) ]; then
  echo "The build cache has grown past ${limit_gb} GB; starting it afresh." >&2
  find /cache/target -mindepth 1 -maxdepth 1 -exec rm -rf {} +
fi

# Work on a private copy, so the checkout stays untouched and host build outputs (target/,
# node_modules/, dist/) never mix with these. tar keeps modification times, so cargo's cache in the
# target volume stays valid between runs.
W=/work/treasury
mkdir -p "$W"
tar -C /src --exclude=./.git --exclude=./target --exclude=./crates/ui/dist \
  --exclude=./site/node_modules --exclude=./site/book --exclude=./.work -cf - . | tar -C "$W" -xf -
cd "$W"

[ "$#" -gt 0 ] || set -- ci
case "$1" in
  shell) exec bash ;;
  bundle)
    # The desktop app's Linux packages, copied to /out (see `scripts/docker-check.sh bundle`).
    cd crates/app
    cargo tauri build --ci --bundles deb,rpm --config '{"bundle":{"createUpdaterArtifacts":false}}'
    find "$CARGO_TARGET_DIR/release/bundle" -maxdepth 2 -type f \( -name '*.deb' -o -name '*.rpm' \) \
      -exec cp {} /out/ \;
    ls -l /out
    ;;
  --)
    shift
    exec "$@"
    ;;
  -h | --help | help) usage ;;
  *) exec cargo xtask "$@" ;;
esac
